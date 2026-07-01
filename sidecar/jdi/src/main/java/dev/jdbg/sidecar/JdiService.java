package dev.jdbg.sidecar;

import com.sun.jdi.AbsentInformationException;
import com.sun.jdi.ArrayReference;
import com.sun.jdi.Bootstrap;
import com.sun.jdi.Field;
import com.sun.jdi.IncompatibleThreadStateException;
import com.sun.jdi.Location;
import com.sun.jdi.ObjectReference;
import com.sun.jdi.ReferenceType;
import com.sun.jdi.StackFrame;
import com.sun.jdi.StringReference;
import com.sun.jdi.ThreadGroupReference;
import com.sun.jdi.ThreadReference;
import com.sun.jdi.Value;
import com.sun.jdi.VirtualMachine;
import com.sun.jdi.connect.AttachingConnector;
import com.sun.jdi.connect.Connector;
import com.sun.jdi.event.BreakpointEvent;
import com.sun.jdi.event.ClassPrepareEvent;
import com.sun.jdi.event.Event;
import com.sun.jdi.event.EventIterator;
import com.sun.jdi.event.EventSet;
import com.sun.jdi.event.StepEvent;
import com.sun.jdi.event.VMDeathEvent;
import com.sun.jdi.event.VMDisconnectEvent;
import com.sun.jdi.event.VMStartEvent;
import com.sun.jdi.request.BreakpointRequest;
import com.sun.jdi.request.ClassPrepareRequest;
import com.sun.jdi.request.EventRequest;
import com.sun.jdi.request.StepRequest;

import java.util.ArrayList;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.LinkedBlockingQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicLong;

final class JdiService {
    private final FrameConnection connection;
    private final Map<String, DebugSession> sessions = new ConcurrentHashMap<>();
    private final AtomicLong eventSeq = new AtomicLong(1);

    JdiService(FrameConnection connection) {
        this.connection = connection;
    }

    Object call(String method, Map<String, Object> params) throws Exception {
        switch (method) {
            case "ping":
                return Json.object("ok", true, "serverVersion", SidecarMain.VERSION);
            case "attach":
                return attach(params);
            case "detach":
                return detach(params);
            case "threads":
                return session(params).threads();
            case "stack":
                return session(params).stack(Json.intValue(params, "maxFrames", 64));
            case "stacks":
                return session(params).stacks(Json.intValue(params, "maxFrames", 64));
            case "locals":
                return session(params).locals();
            case "setBreakpoint":
                return session(params).setBreakpoint(params);
            case "continue":
                return session(params).continueFor(Json.longValue(params, "timeoutMs", 30000));
            case "stepOver":
                return session(params).stepOver(Json.longValue(params, "timeoutMs", 30000));
            case "selectThread":
                return session(params).selectThread(Json.string(params, "threadId"));
            case "inspect":
                return session(params).inspect(params);
            case "shutdown":
                shutdown();
                return Json.object("ok", true);
            default:
                throw new RpcException("unknown_method", "unknown method: " + method);
        }
    }

    private Object attach(Map<String, Object> params) throws Exception {
        String id = Json.string(params, "session");
        String host = Json.optionalString(params, "host", "127.0.0.1");
        int port = Json.intValue(params, "port", 5005);
        AttachingConnector connector = socketAttachConnector();
        Map<String, Connector.Argument> args = connector.defaultArguments();
        args.get("hostname").setValue(host);
        args.get("port").setValue(Integer.toString(port));
        VirtualMachine vm = connector.attach(args);
        DebugSession session = new DebugSession(id, vm);
        sessions.put(id, session);
        session.startEventLoop();
        return Json.object("ok", true, "session", id);
    }

    private Object detach(Map<String, Object> params) throws Exception {
        DebugSession session = session(params);
        sessions.remove(session.id);
        session.detach();
        return Json.object("ok", true);
    }

    private DebugSession session(Map<String, Object> params) throws RpcException {
        String id = Json.string(params, "session");
        DebugSession session = sessions.get(id);
        if (session == null) {
            throw new RpcException("session_not_found", "JDI session not found: " + id);
        }
        return session;
    }

    private void shutdown() {
        for (DebugSession session : sessions.values()) {
            try {
                session.detach();
            } catch (Exception ignored) {
            }
        }
        sessions.clear();
    }

    private static AttachingConnector socketAttachConnector() throws RpcException {
        for (AttachingConnector connector : Bootstrap.virtualMachineManager().attachingConnectors()) {
            if ("com.sun.jdi.SocketAttach".equals(connector.name())) {
                return connector;
            }
        }
        throw new RpcException("connector_not_found", "JDI SocketAttach connector not found");
    }

    private final class DebugSession {
        final String id;
        final VirtualMachine vm;
        final BlockingQueue<Map<String, Object>> stops = new LinkedBlockingQueue<>();
        final List<PendingBreakpoint> pendingBreakpoints = new ArrayList<>();
        volatile String currentThreadId;
        volatile EventSet currentStopSet;
        volatile boolean disconnected;

        DebugSession(String id, VirtualMachine vm) {
            this.id = id;
            this.vm = vm;
        }

        void startEventLoop() {
            Thread thread = new Thread(this::eventLoop, "jdbg-jdi-events-" + id);
            thread.setDaemon(true);
            thread.start();
        }

        Object threads() {
            List<Object> threads = Json.array();
            for (ThreadReference thread : vm.allThreads()) {
                String state = threadState(thread);
                if (Long.toString(thread.uniqueID()).equals(currentThreadId)) {
                    state = state + " (at breakpoint)";
                }
                threads.add(Json.object(
                        "id", Long.toString(thread.uniqueID()),
                        "name", thread.name(),
                        "group", groupName(thread),
                        "state", state
                ));
            }
            return Json.object("threads", threads);
        }

        Object stack(int maxFrames) throws RpcException {
            ThreadReference thread = currentThread();
            return Json.object("frames", frames(thread, maxFrames));
        }

        Object stacks(int maxFrames) {
            List<Object> out = Json.array();
            for (ThreadReference thread : vm.allThreads()) {
                List<Object> frames;
                try {
                    frames = frames(thread, maxFrames);
                } catch (RpcException e) {
                    frames = Json.array();
                }
                out.add(Json.object("thread", thread.name(), "frames", frames));
            }
            return Json.object("threads", out);
        }

        Object locals() throws RpcException {
            StackFrame frame = currentFrame();
            try {
                List<Object> vars = Json.array();
                Map<com.sun.jdi.LocalVariable, Value> values = frame.getValues(frame.visibleVariables());
                for (Map.Entry<com.sun.jdi.LocalVariable, Value> entry : values.entrySet()) {
                    vars.add(Json.object(
                            "name", entry.getKey().name(),
                            "ty", entry.getKey().typeName(),
                            "value", ValueRenderer.display(entry.getValue())
                    ));
                }
                return Json.object("vars", vars);
            } catch (AbsentInformationException e) {
                return Json.object(
                        "vars", Json.array(),
                        "note", "Local variable information not available; compile classes with javac -g."
                );
            }
        }

        Object setBreakpoint(Map<String, Object> params) throws Exception {
            String className = Json.string(params, "class");
            int line = Json.intValue(params, "line", 0);
            String suspend = Json.optionalString(params, "suspend", "all");
            int suspendPolicy = "thread".equals(suspend)
                    ? EventRequest.SUSPEND_EVENT_THREAD
                    : EventRequest.SUSPEND_ALL;
            List<ReferenceType> loaded = vm.classesByName(className);
            if (!loaded.isEmpty()) {
                createBreakpoint(loaded.get(0), line, suspendPolicy);
                return Json.object("spec", className + ":" + line, "deferred", false);
            }

            PendingBreakpoint pending = new PendingBreakpoint(className, line, suspendPolicy);
            pendingBreakpoints.add(pending);
            ClassPrepareRequest request = vm.eventRequestManager().createClassPrepareRequest();
            request.addClassFilter(className);
            request.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            request.enable();
            pending.prepareRequest = request;
            return Json.object("spec", className + ":" + line, "deferred", true);
        }

        Object continueFor(long timeoutMs) throws RpcException, InterruptedException {
            long deadline = System.currentTimeMillis() + timeoutMs;
            stops.clear();
            resumeFromStop();
            while (true) {
                long remaining = deadline - System.currentTimeMillis();
                if (remaining <= 0) {
                    return Json.object("timedOut", true, "partialOutput", "", "state", "running");
                }
                Map<String, Object> stop = stops.poll(remaining, TimeUnit.MILLISECONDS);
                if (stop == null) {
                    return Json.object("timedOut", true, "partialOutput", "", "state", "running");
                }
                if ("vmStart".equals(stop.get("event"))) {
                    resumeFromStop();
                    continue;
                }
                return stop;
            }
        }

        Object stepOver(long timeoutMs) throws Exception {
            ThreadReference thread = currentThread();
            StepRequest request = vm.eventRequestManager().createStepRequest(
                    thread,
                    StepRequest.STEP_LINE,
                    StepRequest.STEP_OVER
            );
            request.addCountFilter(1);
            request.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            request.enable();
            return continueFor(timeoutMs);
        }

        Object selectThread(String id) throws RpcException {
            findThread(id);
            currentThreadId = id;
            return Json.object("ok", true);
        }

        Object inspect(Map<String, Object> params) throws RpcException {
            String expr = Json.string(params, "expr");
            Map<String, Object> limits = Json.asObject(params.get("limits"), "limits");
            Value value = resolveExpression(expr);
            return Json.object(
                    "expr", expr,
                    "value", ValueRenderer.render(value, limits)
            );
        }

        void detach() {
            try {
                resumeFromStop();
            } catch (Exception ignored) {
            }
            try {
                vm.dispose();
            } catch (Exception ignored) {
            }
            disconnected = true;
        }

        private void eventLoop() {
            while (!disconnected) {
                EventSet eventSet;
                try {
                    eventSet = vm.eventQueue().remove();
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    return;
                } catch (Exception e) {
                    queueVmDisconnected("event loop ended: " + e.getMessage());
                    return;
                }

                boolean shouldResume = true;
                EventIterator it = eventSet.eventIterator();
                while (it.hasNext()) {
                    Event event = it.nextEvent();
                    try {
                        if (event instanceof VMStartEvent) {
                            shouldResume = false;
                            handleVmStart(eventSet);
                        } else if (event instanceof ClassPrepareEvent) {
                            resolvePending((ClassPrepareEvent) event);
                        } else if (event instanceof BreakpointEvent) {
                            shouldResume = false;
                            handleStop(eventSet, "breakpoint", ((BreakpointEvent) event).thread(), ((BreakpointEvent) event).location(), null);
                        } else if (event instanceof StepEvent) {
                            shouldResume = false;
                            StepEvent step = (StepEvent) event;
                            vm.eventRequestManager().deleteEventRequest(step.request());
                            handleStop(eventSet, "step", step.thread(), step.location(), null);
                        } else if (event instanceof VMDisconnectEvent || event instanceof VMDeathEvent) {
                            disconnected = true;
                            shouldResume = false;
                            queueVmDisconnected("target VM disconnected");
                        }
                    } catch (Exception e) {
                        System.err.println("jdbg sidecar event error: " + e.getMessage());
                    }
                }
                if (shouldResume) {
                    eventSet.resume();
                }
            }
        }

        private void handleStop(EventSet eventSet, String kind, ThreadReference thread, Location location, String note) {
            currentStopSet = eventSet;
            currentThreadId = Long.toString(thread.uniqueID());
            Map<String, Object> stop = stopPayload(kind, thread, location, note);
            stops.offer(stop);
            sendEvent(kind, stop);
        }

        private void handleVmStart(EventSet eventSet) {
            currentStopSet = eventSet;
            Map<String, Object> stop = Json.object(
                    "event", "vmStart",
                    "thread", "",
                    "threadId", null,
                    "location", Json.object("class", "", "method", "", "file", null, "line", 0),
                    "note", "target VM is suspended at startup"
            );
            stops.offer(stop);
            sendEvent("vmStart", stop);
        }

        private Map<String, Object> stopPayload(String kind, ThreadReference thread, Location location, String note) {
            Map<String, Object> out = new LinkedHashMap<>();
            out.put("event", kind);
            out.put("thread", thread.name());
            out.put("threadId", Long.toString(thread.uniqueID()));
            out.put("location", locationMap(location));
            try {
                List<Object> frames = frames(thread, 1);
                if (!frames.isEmpty()) {
                    out.put("frame", frames.get(0));
                }
            } catch (RpcException ignored) {
            }
            if (note != null) {
                out.put("note", note);
            }
            return out;
        }

        private void queueVmDisconnected(String message) {
            Map<String, Object> payload = Json.object(
                    "event", "vmDisconnected",
                    "thread", "",
                    "threadId", null,
                    "location", Json.object("class", "", "method", "", "file", null, "line", 0),
                    "message", message
            );
            stops.offer(payload);
            sendEvent("vmDisconnected", Json.object("message", message));
        }

        private void sendEvent(String event, Object payload) {
            try {
                connection.writeMessage(Json.object(
                        "type", "event",
                        "session", id,
                        "seq", eventSeq.getAndIncrement(),
                        "event", event,
                        "payload", payload
                ));
            } catch (Exception e) {
                System.err.println("jdbg sidecar failed to send event: " + e.getMessage());
            }
        }

        private void resumeFromStop() {
            EventSet stopSet = currentStopSet;
            currentStopSet = null;
            if (stopSet != null) {
                stopSet.resume();
            } else {
                vm.resume();
            }
        }

        private void resolvePending(ClassPrepareEvent event) throws Exception {
            String name = event.referenceType().name();
            Iterator<PendingBreakpoint> it = pendingBreakpoints.iterator();
            while (it.hasNext()) {
                PendingBreakpoint pending = it.next();
                if (pending.className.equals(name)) {
                    createBreakpoint(event.referenceType(), pending.line, pending.suspendPolicy);
                    if (pending.prepareRequest != null) {
                        vm.eventRequestManager().deleteEventRequest(pending.prepareRequest);
                    }
                    it.remove();
                }
            }
        }

        private void createBreakpoint(ReferenceType type, int line, int suspendPolicy) throws Exception {
            List<Location> locations = type.locationsOfLine(line);
            if (locations.isEmpty()) {
                throw new RpcException("no_executable_line", "no executable location at " + type.name() + ":" + line);
            }
            BreakpointRequest request = vm.eventRequestManager().createBreakpointRequest(locations.get(0));
            request.setSuspendPolicy(suspendPolicy);
            request.enable();
        }

        private ThreadReference currentThread() throws RpcException {
            if (currentThreadId != null) {
                return findThread(currentThreadId);
            }
            for (ThreadReference thread : vm.allThreads()) {
                try {
                    if (thread.isSuspended() && thread.frameCount() > 0) {
                        currentThreadId = Long.toString(thread.uniqueID());
                        return thread;
                    }
                } catch (IncompatibleThreadStateException ignored) {
                }
            }
            throw new RpcException("no_current_thread", "no suspended thread with stack frames is selected");
        }

        private ThreadReference findThread(String id) throws RpcException {
            for (ThreadReference thread : vm.allThreads()) {
                if (Long.toString(thread.uniqueID()).equals(id)) {
                    return thread;
                }
            }
            throw new RpcException("thread_not_found", "thread not found: " + id);
        }

        private StackFrame currentFrame() throws RpcException {
            ThreadReference thread = currentThread();
            try {
                if (thread.frameCount() == 0) {
                    throw new RpcException("empty_stack", "current thread has no stack frames");
                }
                return thread.frame(0);
            } catch (IncompatibleThreadStateException e) {
                throw new RpcException("thread_not_suspended", "current thread is not suspended", e);
            }
        }

        private List<Object> frames(ThreadReference thread, int maxFrames) throws RpcException {
            try {
                List<Object> frames = Json.array();
                int count = Math.min(thread.frameCount(), maxFrames);
                for (int i = 0; i < count; i++) {
                    frames.add(frameMap(i, thread.frame(i)));
                }
                return frames;
            } catch (IncompatibleThreadStateException e) {
                throw new RpcException("thread_not_suspended", "thread is not suspended: " + thread.name(), e);
            }
        }

        private Value resolveExpression(String expr) throws RpcException {
            StackFrame frame = currentFrame();
            String[] parts = expr.split("\\.");
            if (parts.length == 0) {
                throw new RpcException("bad_expression", "empty expression");
            }
            Value value = firstExpressionValue(frame, parts[0]);
            for (int i = 1; i < parts.length; i++) {
                value = fieldValue(value, parts[i]);
            }
            return value;
        }

        private Value firstExpressionValue(StackFrame frame, String name) throws RpcException {
            try {
                if ("this".equals(name)) {
                    return frame.thisObject();
                }
                for (com.sun.jdi.LocalVariable variable : frame.visibleVariables()) {
                    if (variable.name().equals(name)) {
                        return frame.getValue(variable);
                    }
                }
            } catch (AbsentInformationException e) {
                throw new RpcException("locals_unavailable", "local variable information unavailable; compile with javac -g", e);
            }
            throw new RpcException("name_not_found", "name not found in current frame: " + name);
        }

        private Value fieldValue(Value value, String fieldName) throws RpcException {
            if (!(value instanceof ObjectReference)) {
                throw new RpcException("not_object", "cannot read field '" + fieldName + "' from non-object value");
            }
            ObjectReference object = (ObjectReference) value;
            for (Field field : object.referenceType().allFields()) {
                if (field.name().equals(fieldName)) {
                    return object.getValue(field);
                }
            }
            throw new RpcException("field_not_found", "field not found: " + fieldName);
        }
    }

    private static Map<String, Object> frameMap(int index, StackFrame frame) {
        return Json.object(
                "index", index,
                "location", locationMap(frame.location()),
                "is_native", frame.location().method().isNative()
        );
    }

    private static Map<String, Object> locationMap(Location location) {
        String file = null;
        try {
            file = location.sourceName();
        } catch (AbsentInformationException ignored) {
        }
        return Json.object(
                "class", location.declaringType().name(),
                "method", location.method().name(),
                "file", file,
                "line", Math.max(location.lineNumber(), 0)
        );
    }

    private static String groupName(ThreadReference thread) {
        try {
            ThreadGroupReference group = thread.threadGroup();
            return group == null ? null : group.name();
        } catch (Exception e) {
            return null;
        }
    }

    private static String threadState(ThreadReference thread) {
        switch (thread.status()) {
            case ThreadReference.THREAD_STATUS_MONITOR:
                return "monitor";
            case ThreadReference.THREAD_STATUS_NOT_STARTED:
                return "not started";
            case ThreadReference.THREAD_STATUS_RUNNING:
                return thread.isSuspended() ? "suspended" : "running";
            case ThreadReference.THREAD_STATUS_SLEEPING:
                return "sleeping";
            case ThreadReference.THREAD_STATUS_UNKNOWN:
                return thread.isSuspended() ? "suspended" : "unknown";
            case ThreadReference.THREAD_STATUS_WAIT:
                return "waiting";
            case ThreadReference.THREAD_STATUS_ZOMBIE:
                return "zombie";
            default:
                return "unknown";
        }
    }

    private static final class PendingBreakpoint {
        final String className;
        final int line;
        final int suspendPolicy;
        ClassPrepareRequest prepareRequest;

        PendingBreakpoint(String className, int line, int suspendPolicy) {
            this.className = className;
            this.line = line;
            this.suspendPolicy = suspendPolicy;
        }
    }
}
