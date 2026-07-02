package dev.jdbg.sidecar;

import com.sun.jdi.AbsentInformationException;
import com.sun.jdi.ArrayReference;
import com.sun.jdi.Bootstrap;
import com.sun.jdi.Field;
import com.sun.jdi.IncompatibleThreadStateException;
import com.sun.jdi.Location;
import com.sun.jdi.Method;
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
import com.sun.jdi.connect.LaunchingConnector;
import com.sun.jdi.event.AccessWatchpointEvent;
import com.sun.jdi.event.BreakpointEvent;
import com.sun.jdi.event.ClassPrepareEvent;
import com.sun.jdi.event.Event;
import com.sun.jdi.event.EventIterator;
import com.sun.jdi.event.EventSet;
import com.sun.jdi.event.ExceptionEvent;
import com.sun.jdi.event.MethodEntryEvent;
import com.sun.jdi.event.MethodExitEvent;
import com.sun.jdi.event.ModificationWatchpointEvent;
import com.sun.jdi.event.StepEvent;
import com.sun.jdi.event.WatchpointEvent;
import com.sun.jdi.event.VMDeathEvent;
import com.sun.jdi.event.VMDisconnectEvent;
import com.sun.jdi.event.VMStartEvent;
import com.sun.jdi.request.AccessWatchpointRequest;
import com.sun.jdi.request.BreakpointRequest;
import com.sun.jdi.request.ClassPrepareRequest;
import com.sun.jdi.request.EventRequest;
import com.sun.jdi.request.ExceptionRequest;
import com.sun.jdi.request.MethodEntryRequest;
import com.sun.jdi.request.MethodExitRequest;
import com.sun.jdi.request.ModificationWatchpointRequest;
import com.sun.jdi.request.StepRequest;

import java.io.IOException;
import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
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
            case "launch":
                return launch(params);
            case "attach":
                return attach(params);
            case "terminate":
                return terminate(params);
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
            case "breakpoints":
                return session(params).breakpoints();
            case "clearBreakpoint":
                return session(params).clearBreakpoint(params);
            case "setMethodEvent":
                return session(params).setMethodEvent(params);
            case "catchException":
                return session(params).catchException(params);
            case "ignoreException":
                return session(params).ignoreException(params);
            case "setWatchpoint":
                return session(params).setWatchpoint(params);
            case "clearWatchpoint":
                return session(params).clearWatchpoint(params);
            case "continue":
                return session(params).continueFor(Json.longValue(params, "timeoutMs", 30000));
            case "stepInto":
                return session(params).stepInto(Json.longValue(params, "timeoutMs", 30000));
            case "stepOver":
                return session(params).stepOver(Json.longValue(params, "timeoutMs", 30000));
            case "stepOut":
                return session(params).stepOut(Json.longValue(params, "timeoutMs", 30000));
            case "classes":
                return session(params).classes(Json.optionalString(params, "pattern", null));
            case "methods":
                return session(params).methods(Json.string(params, "class"));
            case "selectThread":
                return session(params).selectThread(Json.string(params, "threadId"));
            case "selectFrame":
                return session(params).selectFrame(
                        Json.string(params, "direction"),
                        Json.intValue(params, "count", 1)
                );
            case "listSource":
                return session(params).listSource(optionalInteger(params, "line"));
            case "inspect":
                return session(params).inspect(params);
            case "evaluateExpression":
                return session(params).evaluateExpression(params);
            case "renderExpression":
                return session(params).renderExpression(params);
            case "setValue":
                return session(params).setValue(params);
            case "forceReturn":
                return session(params).forceReturn(params);
            case "suspend":
                return session(params).suspendThread(Json.optionalString(params, "threadId", null));
            case "resume":
                return session(params).resumeThread(Json.optionalString(params, "threadId", null));
            case "lockInfo":
                return session(params).lockInfo(Json.string(params, "expr"));
            case "threadLocks":
                return session(params).threadLocks(Json.optionalString(params, "threadId", null));
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
        DebugSession session = new DebugSession(id, vm, stringList(params, "sourcepath"));
        sessions.put(id, session);
        session.startEventLoop();
        return Json.object("ok", true, "session", id);
    }

    private Object launch(Map<String, Object> params) throws Exception {
        String id = Json.string(params, "session");
        String mainClass = Json.string(params, "mainClass");
        List<String> classpath = stringList(params, "classpath");
        List<String> appArgs = stringList(params, "appArgs");

        LaunchingConnector connector = commandLineLaunchConnector();
        Map<String, Connector.Argument> args = connector.defaultArguments();
        args.get("main").setValue(buildLaunchMainArgument(mainClass, appArgs));
        args.get("suspend").setValue("true");
        String options = buildLaunchOptions(classpath);
        if (!options.isEmpty()) {
            args.get("options").setValue(options);
        }
        VirtualMachine vm = connector.launch(args);
        DebugSession session = new DebugSession(id, vm, stringList(params, "sourcepath"));
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

    private Object terminate(Map<String, Object> params) throws Exception {
        DebugSession session = session(params);
        sessions.remove(session.id);
        session.terminate();
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

    private static LaunchingConnector commandLineLaunchConnector() throws RpcException {
        for (LaunchingConnector connector : Bootstrap.virtualMachineManager().launchingConnectors()) {
            if ("com.sun.jdi.CommandLineLaunch".equals(connector.name())) {
                return connector;
            }
        }
        throw new RpcException("connector_not_found", "JDI CommandLineLaunch connector not found");
    }

    static String buildLaunchMainArgument(String mainClass, List<String> appArgs) {
        StringBuilder out = new StringBuilder(mainClass);
        for (String arg : appArgs) {
            out.append(' ').append(quoteLaunchArgument(arg));
        }
        return out.toString();
    }

    private static String buildLaunchOptions(List<String> classpath) {
        if (classpath.isEmpty()) {
            return "";
        }
        StringBuilder joined = new StringBuilder();
        for (int i = 0; i < classpath.size(); i++) {
            if (i > 0) {
                joined.append(File.pathSeparator);
            }
            joined.append(classpath.get(i));
        }
        return "-cp " + quoteLaunchArgument(joined.toString());
    }

    private static String quoteLaunchArgument(String value) {
        if (value.length() > 0 && value.matches("[A-Za-z0-9_./:=+-]+")) {
            return value;
        }
        String escaped = value.replace("\\", "\\\\").replace("\"", "\\\"");
        return "\"" + escaped + "\"";
    }

    private static List<String> stringList(Map<String, Object> params, String key) {
        Object value = params.get(key);
        List<String> out = new ArrayList<>();
        if (value == null) {
            return out;
        }
        for (Object item : Json.asList(value, key)) {
            if (item instanceof String) {
                out.add((String) item);
            } else {
                throw new IllegalArgumentException(key + " entries must be strings");
            }
        }
        return out;
    }

    private static Integer optionalInteger(Map<String, Object> params, String key) {
        Object value = params.get(key);
        if (value == null) {
            return null;
        }
        return Json.intValue(params, key, 0);
    }

    private final class DebugSession {
        final String id;
        final VirtualMachine vm;
        final List<String> sourcePaths;
        final BlockingQueue<StopRecord> stops = new LinkedBlockingQueue<>();
        final List<PendingBreakpoint> pendingBreakpoints = new ArrayList<>();
        final List<ActiveBreakpoint> activeBreakpoints = new ArrayList<>();
        final List<PendingWatchpoint> pendingWatchpoints = new ArrayList<>();
        final List<ActiveWatchpoint> activeWatchpoints = new ArrayList<>();
        final List<ActiveMethodEvent> activeMethodEvents = new ArrayList<>();
        final List<ActiveExceptionRequest> activeExceptionRequests = new ArrayList<>();
        volatile String currentThreadId;
        volatile int currentFrameIndex;
        volatile EventSet currentStopSet;
        volatile boolean waitingForStopResponse;
        volatile boolean disconnected;

        DebugSession(String id, VirtualMachine vm, List<String> sourcePaths) {
            this.id = id;
            this.vm = vm;
            this.sourcePaths = sourcePaths;
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
                if (currentStopSet != null && Long.toString(thread.uniqueID()).equals(currentThreadId)) {
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
                createBreakpoint(loaded.get(0), line, suspendPolicy, className + ":" + line);
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

        Object breakpoints() {
            List<Object> out = Json.array();
            for (PendingBreakpoint pending : pendingBreakpoints) {
                out.add(pending.spec() + " (deferred)");
            }
            for (ActiveBreakpoint active : activeBreakpoints) {
                out.add(active.spec);
            }
            for (ActiveMethodEvent active : activeMethodEvents) {
                out.add(active.spec.display() + " (" + active.eventKind + ")");
            }
            for (PendingWatchpoint pending : pendingWatchpoints) {
                out.add("watch " + pending.field.spec + " (" + pending.mode + ", deferred)");
            }
            for (ActiveWatchpoint active : activeWatchpoints) {
                out.add("watch " + active.field.spec + " (" + active.mode + ")");
            }
            for (ActiveExceptionRequest active : activeExceptionRequests) {
                out.add("catch " + active.exceptionName + " (" + active.mode + ")");
            }
            return Json.object("breakpoints", out);
        }

        Object clearBreakpoint(Map<String, Object> params) throws Exception {
            String spec = Json.string(params, "spec");
            int removed = 0;

            Iterator<PendingBreakpoint> pendingIt = pendingBreakpoints.iterator();
            while (pendingIt.hasNext()) {
                PendingBreakpoint pending = pendingIt.next();
                if (pending.matches(spec)) {
                    if (pending.prepareRequest != null) {
                        vm.eventRequestManager().deleteEventRequest(pending.prepareRequest);
                    }
                    pendingIt.remove();
                    removed++;
                }
            }

            Iterator<ActiveBreakpoint> activeIt = activeBreakpoints.iterator();
            while (activeIt.hasNext()) {
                ActiveBreakpoint active = activeIt.next();
                if (active.matches(spec)) {
                    vm.eventRequestManager().deleteEventRequest(active.request);
                    activeIt.remove();
                    removed++;
                }
            }

            Iterator<ActiveMethodEvent> methodIt = activeMethodEvents.iterator();
            while (methodIt.hasNext()) {
                ActiveMethodEvent active = methodIt.next();
                if (active.matches(spec)) {
                    vm.eventRequestManager().deleteEventRequest(active.request);
                    methodIt.remove();
                    removed++;
                }
            }

            Iterator<PendingWatchpoint> pendingWatchIt = pendingWatchpoints.iterator();
            while (pendingWatchIt.hasNext()) {
                PendingWatchpoint pending = pendingWatchIt.next();
                if (pending.field.spec.equals(spec)) {
                    if (pending.prepareRequest != null) {
                        vm.eventRequestManager().deleteEventRequest(pending.prepareRequest);
                    }
                    pendingWatchIt.remove();
                    removed++;
                }
            }

            Iterator<ActiveWatchpoint> activeWatchIt = activeWatchpoints.iterator();
            while (activeWatchIt.hasNext()) {
                ActiveWatchpoint active = activeWatchIt.next();
                if (active.field.spec.equals(spec)) {
                    vm.eventRequestManager().deleteEventRequest(active.request);
                    activeWatchIt.remove();
                    removed++;
                }
            }

            Iterator<ActiveExceptionRequest> exceptionIt = activeExceptionRequests.iterator();
            while (exceptionIt.hasNext()) {
                ActiveExceptionRequest active = exceptionIt.next();
                if (active.exceptionName.equals(spec)) {
                    vm.eventRequestManager().deleteEventRequest(active.request);
                    exceptionIt.remove();
                    removed++;
                }
            }

            if (removed == 0) {
                throw new RpcException("breakpoint_not_found", "breakpoint not found: " + spec);
            }
            return Json.object("removed", removed, "spec", spec);
        }

        Object setMethodEvent(Map<String, Object> params) throws Exception {
            MethodSpec spec = MethodSpec.from(
                    Json.string(params, "class"),
                    Json.string(params, "method"),
                    Json.optionalString(params, "args", null),
                    Json.optionalString(params, "event", "entry"),
                    Json.optionalString(params, "suspend", "all")
            );
            int suspendPolicy = "thread".equals(spec.suspend)
                    ? EventRequest.SUSPEND_EVENT_THREAD
                    : EventRequest.SUSPEND_ALL;
            if ("entry".equals(spec.eventKind) || "both".equals(spec.eventKind)) {
                MethodEntryRequest request = vm.eventRequestManager().createMethodEntryRequest();
                request.addClassFilter(spec.className);
                request.setSuspendPolicy(suspendPolicy);
                request.putProperty("jdbg.methodSpec", spec);
                request.enable();
                activeMethodEvents.add(new ActiveMethodEvent(spec, "entry", request));
            }
            if ("exit".equals(spec.eventKind) || "both".equals(spec.eventKind)) {
                MethodExitRequest request = vm.eventRequestManager().createMethodExitRequest();
                request.addClassFilter(spec.className);
                request.setSuspendPolicy(suspendPolicy);
                request.putProperty("jdbg.methodSpec", spec);
                request.enable();
                activeMethodEvents.add(new ActiveMethodEvent(spec, "exit", request));
            }
            return Json.object(
                    "spec", spec.display(),
                    "deferred", vm.classesByName(spec.className).isEmpty(),
                    "note", "JDI method " + spec.eventKind + " event request installed"
            );
        }

        Object catchException(Map<String, Object> params) throws Exception {
            String exceptionName = Json.string(params, "exception");
            String mode = Json.optionalString(params, "mode", "all");
            boolean caught = "caught".equals(mode) || "all".equals(mode);
            boolean uncaught = "uncaught".equals(mode) || "all".equals(mode);
            if (!caught && !uncaught) {
                throw new RpcException("bad_catch_mode", "catch mode must be caught, uncaught, or all: " + mode);
            }

            ReferenceType exceptionType = null;
            List<ReferenceType> loaded = vm.classesByName(exceptionName);
            if (!loaded.isEmpty()) {
                exceptionType = loaded.get(0);
            }
            ExceptionRequest request = vm.eventRequestManager().createExceptionRequest(exceptionType, caught, uncaught);
            request.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            request.putProperty("jdbg.exceptionName", exceptionName);
            request.putProperty("jdbg.exceptionMode", mode);
            request.enable();
            activeExceptionRequests.add(new ActiveExceptionRequest(exceptionName, mode, request));
            String note = exceptionType == null
                    ? "Exception type is not loaded yet; JDI will filter matching exceptions at throw time."
                    : null;
            return Json.object("spec", exceptionName, "deferred", exceptionType == null, "note", note);
        }

        Object ignoreException(Map<String, Object> params) throws Exception {
            String exceptionName = Json.string(params, "exception");
            String mode = Json.optionalString(params, "mode", "all");
            int removed = 0;
            Iterator<ActiveExceptionRequest> it = activeExceptionRequests.iterator();
            while (it.hasNext()) {
                ActiveExceptionRequest active = it.next();
                if (active.matches(exceptionName, mode)) {
                    vm.eventRequestManager().deleteEventRequest(active.request);
                    it.remove();
                    removed++;
                }
            }
            if (removed == 0) {
                throw new RpcException("catchpoint_not_found", "catchpoint not found: " + exceptionName + " (" + mode + ")");
            }
            return Json.object("removed", removed, "spec", exceptionName, "mode", mode);
        }

        Object setWatchpoint(Map<String, Object> params) throws Exception {
            String spec = Json.string(params, "field");
            String mode = Json.optionalString(params, "mode", "modification");
            FieldSpec fieldSpec = parseFieldSpec(spec);
            validateWatchMode(mode);

            List<ReferenceType> loaded = vm.classesByName(fieldSpec.className);
            if (!loaded.isEmpty()) {
                createWatchpoints(loaded.get(0), fieldSpec, mode);
                return Json.object("spec", spec, "mode", mode, "deferred", false);
            }

            PendingWatchpoint pending = new PendingWatchpoint(fieldSpec, mode);
            pendingWatchpoints.add(pending);
            ClassPrepareRequest request = vm.eventRequestManager().createClassPrepareRequest();
            request.addClassFilter(fieldSpec.className);
            request.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            request.enable();
            pending.prepareRequest = request;
            return Json.object("spec", spec, "mode", mode, "deferred", true);
        }

        Object clearWatchpoint(Map<String, Object> params) throws Exception {
            String spec = Json.string(params, "field");
            String mode = Json.optionalString(params, "mode", "modification");
            FieldSpec fieldSpec = parseFieldSpec(spec);
            validateWatchMode(mode);

            Iterator<PendingWatchpoint> it = pendingWatchpoints.iterator();
            while (it.hasNext()) {
                PendingWatchpoint pending = it.next();
                if (pending.removeMode(fieldSpec, mode)) {
                    if (pending.prepareRequest != null) {
                        vm.eventRequestManager().deleteEventRequest(pending.prepareRequest);
                    }
                    it.remove();
                }
            }
            Iterator<ActiveWatchpoint> activeIt = activeWatchpoints.iterator();
            while (activeIt.hasNext()) {
                ActiveWatchpoint active = activeIt.next();
                if (active.matches(fieldSpec, mode)) {
                    vm.eventRequestManager().deleteEventRequest(active.request);
                    activeIt.remove();
                }
            }
            return Json.object("ok", true);
        }

        Object continueFor(long timeoutMs) throws RpcException, InterruptedException {
            long deadline = System.currentTimeMillis() + timeoutMs;
            waitingForStopResponse = true;
            try {
                StopRecord stop = pollQueuedStopOrResume();
                while (true) {
                    if (stop == null) {
                        long remaining = deadline - System.currentTimeMillis();
                        if (remaining <= 0) {
                            return Json.object("timedOut", true, "partialOutput", "", "state", "running");
                        }
                        stop = stops.poll(remaining, TimeUnit.MILLISECONDS);
                        if (stop == null) {
                            return Json.object("timedOut", true, "partialOutput", "", "state", "running");
                        }
                    }
                    if (!stop.deliverToWaitingRequest && !"vmDisconnected".equals(stop.payload.get("event"))) {
                        resumeStopSet(stop.eventSet);
                        stop = null;
                        continue;
                    }
                    if ("vmStart".equals(stop.payload.get("event"))) {
                        resumeStopSet(stop.eventSet);
                        stop = null;
                        continue;
                    }
                    selectStop(stop);
                    return stop.payload;
                }
            } finally {
                waitingForStopResponse = false;
            }
        }

        Object stepInto(long timeoutMs) throws Exception {
            return step(timeoutMs, StepRequest.STEP_INTO);
        }

        Object stepOver(long timeoutMs) throws Exception {
            return step(timeoutMs, StepRequest.STEP_OVER);
        }

        Object stepOut(long timeoutMs) throws Exception {
            return step(timeoutMs, StepRequest.STEP_OUT);
        }

        Object classes(String pattern) {
            String needle = pattern == null ? null : pattern.toLowerCase();
            List<String> names = new ArrayList<>();
            for (ReferenceType type : vm.allClasses()) {
                String name = type.name();
                if (needle == null || name.toLowerCase().contains(needle)) {
                    names.add(name);
                }
            }
            Collections.sort(names);
            List<Object> out = Json.array();
            out.addAll(names);
            return Json.object("classes", out);
        }

        Object methods(String className) throws RpcException {
            List<ReferenceType> loaded = vm.classesByName(className);
            if (loaded.isEmpty()) {
                throw new RpcException("class_not_loaded", "class is not loaded: " + className);
            }
            ReferenceType type = loaded.get(0);
            List<String> methods = new ArrayList<>();
            for (Method method : type.allMethods()) {
                methods.add(type.name() + "." + method.name() + method.signature() + " : " + method.returnTypeName());
            }
            Collections.sort(methods);
            List<Object> out = Json.array();
            out.addAll(methods);
            return Json.object("class", className, "methods", out);
        }

        private Object step(long timeoutMs, int depth) throws Exception {
            ThreadReference thread = currentThread();
            deleteStepRequests(thread);
            StepRequest request = vm.eventRequestManager().createStepRequest(
                    thread,
                    StepRequest.STEP_LINE,
                    depth
            );
            request.addCountFilter(1);
            request.setSuspendPolicy(EventRequest.SUSPEND_ALL);
            request.enable();
            return continueFor(timeoutMs);
        }

        private void deleteStepRequests(ThreadReference thread) {
            for (StepRequest request : vm.eventRequestManager().stepRequests()) {
                if (request.thread().equals(thread)) {
                    vm.eventRequestManager().deleteEventRequest(request);
                }
            }
        }

        Object selectThread(String id) throws RpcException {
            findThread(id);
            currentThreadId = id;
            currentFrameIndex = 0;
            return Json.object("ok", true);
        }

        Object selectFrame(String direction, int count) throws RpcException {
            if (!"up".equals(direction) && !"down".equals(direction)) {
                throw new RpcException("bad_frame_direction", "frame direction must be up or down: " + direction);
            }
            ThreadReference thread = currentThread();
            int frameCount;
            try {
                frameCount = thread.frameCount();
            } catch (IncompatibleThreadStateException e) {
                throw new RpcException("thread_not_suspended", "current thread is not suspended", e);
            }
            int delta = Math.max(count, 0);
            int next = "up".equals(direction) ? currentFrameIndex + delta : currentFrameIndex - delta;
            if (next < 0) {
                next = 0;
            }
            if (next >= frameCount) {
                next = frameCount - 1;
            }
            currentFrameIndex = next;
            StackFrame frame = frame(thread);
            return Json.object(
                    "index", currentFrameIndex,
                    "frame", frameMap(currentFrameIndex, frame),
                    "text", "Current frame #" + currentFrameIndex + ": " + frame.location().declaringType().name()
                            + "." + frame.location().method().name() + " line " + Math.max(frame.location().lineNumber(), 0)
            );
        }

        Object listSource(Integer requestedLine) throws RpcException {
            StackFrame frame = currentFrame();
            Location location = frame.location();
            int center = requestedLine == null ? Math.max(location.lineNumber(), 1) : requestedLine.intValue();
            Path source = findSourcePath(location);
            List<String> sourceLines;
            try {
                sourceLines = Files.readAllLines(source, StandardCharsets.UTF_8);
            } catch (IOException e) {
                throw new RpcException("source_unreadable", "failed to read source file: " + source, e);
            }
            int start = Math.max(1, center - 5);
            int end = Math.min(sourceLines.size(), center + 5);
            List<Object> lines = Json.array();
            for (int line = start; line <= end; line++) {
                lines.add(Json.object("number", line, "text", sourceLines.get(line - 1)));
            }
            return Json.object("aroundLine", center, "lines", lines);
        }

        Object suspendThread(String id) throws RpcException {
            if (id == null || id.isEmpty()) {
                vm.suspend();
                return Json.object("text", "Suspended all threads");
            }
            ThreadReference thread = findThread(id);
            thread.suspend();
            return Json.object("text", "Suspended thread " + id + " (" + thread.name() + ")");
        }

        Object resumeThread(String id) throws RpcException {
            if (id == null || id.isEmpty()) {
                EventSet stopSet = currentStopSet;
                if (stopSet != null) {
                    resumeStopSet(stopSet);
                } else {
                    vm.resume();
                }
                return Json.object("text", "Resumed all threads");
            }
            ThreadReference thread = findThread(id);
            thread.resume();
            return Json.object("text", "Resumed thread " + id + " (" + thread.name() + ")");
        }

        Object lockInfo(String expr) throws RpcException {
            if (!vm.canGetMonitorInfo()) {
                throw new RpcException("capability_unavailable", "target VM does not expose monitor info");
            }
            Value value = resolveExpression(expr);
            if (!(value instanceof ObjectReference)) {
                throw new RpcException("not_object", "lock target is not an object: " + expr);
            }
            ObjectReference object = (ObjectReference) value;
            StringBuilder text = new StringBuilder();
            text.append(expr).append(" monitor");
            try {
                ThreadReference owner = object.owningThread();
                text.append("\n  owner: ").append(owner == null ? "(none)" : threadLabel(owner));
                text.append("\n  entryCount: ").append(object.entryCount());
                List<ThreadReference> waiters = object.waitingThreads();
                text.append("\n  waiters:");
                if (waiters.isEmpty()) {
                    text.append(" (none)");
                } else {
                    for (ThreadReference waiter : waiters) {
                        text.append("\n    ").append(threadLabel(waiter));
                    }
                }
            } catch (IncompatibleThreadStateException e) {
                throw new RpcException("thread_not_suspended", "monitor information requires a suspended VM", e);
            }
            return Json.object("text", text.toString());
        }

        Object threadLocks(String id) throws RpcException {
            if (!vm.canGetOwnedMonitorInfo() || !vm.canGetCurrentContendedMonitor()) {
                throw new RpcException("capability_unavailable", "target VM does not expose thread lock info");
            }
            ThreadReference thread = id == null || id.isEmpty() ? currentThread() : findThread(id);
            StringBuilder text = new StringBuilder();
            text.append("Thread ").append(threadLabel(thread)).append(" locks:");
            try {
                ObjectReference blockedOn = thread.currentContendedMonitor();
                text.append("\n  blockedOn: ").append(blockedOn == null ? "(none)" : objectLabel(blockedOn));
                List<ObjectReference> owned = thread.ownedMonitors();
                text.append("\n  owned:");
                if (owned.isEmpty()) {
                    text.append(" (none)");
                } else {
                    for (ObjectReference monitor : owned) {
                        text.append("\n    ").append(objectLabel(monitor));
                    }
                }
            } catch (IncompatibleThreadStateException e) {
                throw new RpcException("thread_not_suspended", "thread lock information requires a suspended thread", e);
            }
            return Json.object("text", text.toString());
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

        Object evaluateExpression(Map<String, Object> params) throws RpcException {
            String expr = Json.string(params, "expr");
            EvaluationContext context = evaluationContext();
            Value value = new ExpressionEvaluator(vm, context.thread, context.frame).evaluate(expr);
            return Json.object(
                    "expr", expr,
                    "value", ValueRenderer.display(value),
                    "type", value == null ? null : value.type().name()
            );
        }

        Object renderExpression(Map<String, Object> params) throws RpcException {
            String expr = Json.string(params, "expr");
            Map<String, Object> limits = Json.asObject(params.get("limits"), "limits");
            EvaluationContext context = evaluationContext();
            Value value = new ExpressionEvaluator(vm, context.thread, context.frame).evaluate(expr);
            return Json.object(
                    "expr", expr,
                    "value", ValueRenderer.render(value, limits)
            );
        }

        Object setValue(Map<String, Object> params) throws RpcException {
            String lvalue = Json.string(params, "lvalue");
            String valueExpr = Json.string(params, "value");
            EvaluationContext context = evaluationContext();
            Value value = new ExpressionEvaluator(vm, context.thread, context.frame).setValue(lvalue, valueExpr);
            return Json.object(
                    "lvalue", lvalue,
                    "value", ValueRenderer.display(value),
                    "type", value == null ? null : value.type().name()
            );
        }

        Object forceReturn(Map<String, Object> params) throws RpcException {
            String valueExpr = Json.string(params, "value");
            EvaluationContext context = evaluationContext();
            Value value = new ExpressionEvaluator(vm, context.thread, context.frame).forceReturn(valueExpr);
            return Json.object(
                    "value", ValueRenderer.display(value),
                    "type", value == null ? null : value.type().name()
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

        void terminate() {
            try {
                resumeFromStop();
            } catch (Exception ignored) {
            }
            try {
                vm.exit(0);
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
                        } else if (event instanceof ExceptionEvent) {
                            if (handleExceptionStop(eventSet, (ExceptionEvent) event)) {
                                shouldResume = false;
                            }
                        } else if (event instanceof MethodEntryEvent) {
                            if (handleMethodEntryStop(eventSet, (MethodEntryEvent) event)) {
                                shouldResume = false;
                            }
                        } else if (event instanceof MethodExitEvent) {
                            if (handleMethodExitStop(eventSet, (MethodExitEvent) event)) {
                                shouldResume = false;
                            }
                        } else if (event instanceof ModificationWatchpointEvent) {
                            shouldResume = false;
                            handleWatchStop(eventSet, (WatchpointEvent) event, "modified");
                        } else if (event instanceof AccessWatchpointEvent) {
                            shouldResume = false;
                            handleWatchStop(eventSet, (WatchpointEvent) event, "accessed");
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
            Map<String, Object> stop = stopPayload(kind, thread, location, note);
            synchronized (this) {
                currentStopSet = eventSet;
                currentThreadId = Long.toString(thread.uniqueID());
                currentFrameIndex = 0;
                stops.offer(new StopRecord(stop, eventSet, waitingForStopResponse));
            }
            sendEvent(kind, stop);
        }

        private void handleWatchStop(EventSet eventSet, WatchpointEvent event, String accessType) {
            Map<String, Object> stop = stopPayload("fieldWatch", event.thread(), event.location(), null);
            stop.put("field", event.field().declaringType().name() + "." + event.field().name());
            stop.put("accessType", accessType);
            synchronized (this) {
                currentStopSet = eventSet;
                currentThreadId = Long.toString(event.thread().uniqueID());
                currentFrameIndex = 0;
                stops.offer(new StopRecord(stop, eventSet, waitingForStopResponse));
            }
            sendEvent("fieldWatch", stop);
        }

        private boolean handleExceptionStop(EventSet eventSet, ExceptionEvent event) {
            String exceptionName = event.exception().referenceType().name();
            for (ActiveExceptionRequest active : activeExceptionRequests) {
                if (active.request == event.request() && !active.matchesException(exceptionName)) {
                    return false;
                }
            }
            Map<String, Object> stop = stopPayload("exception", event.thread(), event.location(), null);
            stop.put("exception", exceptionName);
            stop.put("caught", event.catchLocation() != null);
            synchronized (this) {
                currentStopSet = eventSet;
                currentThreadId = Long.toString(event.thread().uniqueID());
                currentFrameIndex = 0;
                stops.offer(new StopRecord(stop, eventSet, waitingForStopResponse));
            }
            sendEvent("exception", stop);
            return true;
        }

        private boolean handleMethodEntryStop(EventSet eventSet, MethodEntryEvent event) {
            MethodSpec spec = methodSpec(event.request());
            if (spec == null || !spec.matches(event.method())) {
                return false;
            }
            handleStop(eventSet, "methodEntry", event.thread(), event.location(), null);
            return true;
        }

        private boolean handleMethodExitStop(EventSet eventSet, MethodExitEvent event) {
            MethodSpec spec = methodSpec(event.request());
            if (spec == null || !spec.matches(event.method())) {
                return false;
            }
            Map<String, Object> stop = stopPayload("methodExit", event.thread(), event.location(), null);
            Value value = event.returnValue();
            stop.put("returnValue", ValueRenderer.display(value));
            stop.put("returnType", value == null ? event.method().returnTypeName() : value.type().name());
            synchronized (this) {
                currentStopSet = eventSet;
                currentThreadId = Long.toString(event.thread().uniqueID());
                currentFrameIndex = 0;
                stops.offer(new StopRecord(stop, eventSet, waitingForStopResponse));
            }
            sendEvent("methodExit", stop);
            return true;
        }

        private MethodSpec methodSpec(EventRequest request) {
            Object value = request.getProperty("jdbg.methodSpec");
            return value instanceof MethodSpec ? (MethodSpec) value : null;
        }

        private void handleVmStart(EventSet eventSet) {
            Map<String, Object> stop = Json.object(
                    "event", "vmStart",
                    "thread", "",
                    "threadId", null,
                    "location", Json.object("class", "", "method", "", "file", null, "line", 0),
                    "note", "target VM is suspended at startup"
            );
            synchronized (this) {
                currentStopSet = eventSet;
                stops.offer(new StopRecord(stop, eventSet, waitingForStopResponse));
            }
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
            synchronized (this) {
                stops.offer(new StopRecord(payload, null, true));
            }
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

        private synchronized StopRecord pollQueuedStopOrResume() {
            StopRecord stop = stops.poll();
            if (stop == null) {
                resumeFromStopLocked();
            }
            return stop;
        }

        private synchronized void selectStop(StopRecord stop) {
            currentStopSet = stop.eventSet;
            Object threadId = stop.payload.get("threadId");
            if (threadId instanceof String) {
                currentThreadId = (String) threadId;
            }
            currentFrameIndex = 0;
        }

        private synchronized void resumeFromStop() {
            resumeFromStopLocked();
        }

        private synchronized void resumeStopSet(EventSet stopSet) {
            resumeStopSetLocked(stopSet);
        }

        private void resumeFromStopLocked() {
            EventSet stopSet = currentStopSet;
            resumeStopSetLocked(stopSet);
        }

        private void resumeStopSetLocked(EventSet stopSet) {
            if (currentStopSet == stopSet) {
                currentStopSet = null;
            }
            // Startup VMStartEvent delivery can race with the first continue call; only resume
            // an EventSet the event loop has actually recorded, otherwise deferred requests can
            // be installed after the VM has already run past their first possible stop site.
            if (stopSet != null) {
                stopSet.resume();
            }
        }

        private void resolvePending(ClassPrepareEvent event) throws Exception {
            String name = event.referenceType().name();
            Iterator<PendingBreakpoint> it = pendingBreakpoints.iterator();
            while (it.hasNext()) {
                PendingBreakpoint pending = it.next();
                if (pending.className.equals(name)) {
                    createBreakpoint(event.referenceType(), pending.line, pending.suspendPolicy, pending.spec());
                    if (pending.prepareRequest != null) {
                        vm.eventRequestManager().deleteEventRequest(pending.prepareRequest);
                    }
                    it.remove();
                }
            }
            Iterator<PendingWatchpoint> watchIt = pendingWatchpoints.iterator();
            while (watchIt.hasNext()) {
                PendingWatchpoint pending = watchIt.next();
                if (pending.field.className.equals(name)) {
                    createWatchpoints(event.referenceType(), pending.field, pending.mode);
                    if (pending.prepareRequest != null) {
                        vm.eventRequestManager().deleteEventRequest(pending.prepareRequest);
                    }
                    watchIt.remove();
                }
            }
        }

        private void createBreakpoint(ReferenceType type, int line, int suspendPolicy, String spec) throws Exception {
            List<Location> locations = type.locationsOfLine(line);
            if (locations.isEmpty()) {
                throw new RpcException("no_executable_line", "no executable location at " + type.name() + ":" + line);
            }
            BreakpointRequest request = vm.eventRequestManager().createBreakpointRequest(locations.get(0));
            request.setSuspendPolicy(suspendPolicy);
            request.enable();
            activeBreakpoints.add(new ActiveBreakpoint(spec, type.name(), line, request));
        }

        private void createWatchpoints(ReferenceType type, FieldSpec fieldSpec, String mode) throws RpcException {
            Field field = findField(type, fieldSpec.fieldName);
            if ("modification".equals(mode) || "all".equals(mode)) {
                ModificationWatchpointRequest request = vm.eventRequestManager().createModificationWatchpointRequest(field);
                request.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                request.enable();
                activeWatchpoints.add(new ActiveWatchpoint(fieldSpec, "modification", request));
            }
            if ("access".equals(mode) || "all".equals(mode)) {
                AccessWatchpointRequest request = vm.eventRequestManager().createAccessWatchpointRequest(field);
                request.setSuspendPolicy(EventRequest.SUSPEND_ALL);
                request.enable();
                activeWatchpoints.add(new ActiveWatchpoint(fieldSpec, "access", request));
            }
        }

        private Field findField(ReferenceType type, String name) throws RpcException {
            for (Field field : type.allFields()) {
                if (field.name().equals(name)) {
                    return field;
                }
            }
            throw new RpcException("field_not_found", "field not found: " + type.name() + "." + name);
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
            return frame(thread);
        }

        private EvaluationContext evaluationContext() throws RpcException {
            ThreadReference thread = currentThread();
            return new EvaluationContext(thread, frame(thread));
        }

        private StackFrame frame(ThreadReference thread) throws RpcException {
            try {
                if (thread.frameCount() == 0) {
                    throw new RpcException("empty_stack", "current thread has no stack frames");
                }
                int index = Math.max(0, Math.min(currentFrameIndex, thread.frameCount() - 1));
                currentFrameIndex = index;
                return thread.frame(index);
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
            if (value instanceof ArrayReference && "length".equals(fieldName)) {
                return vm.mirrorOf(((ArrayReference) value).length());
            }
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

        private Path findSourcePath(Location location) throws RpcException {
            List<String> candidates = new ArrayList<>();
            try {
                candidates.add(location.sourcePath());
            } catch (AbsentInformationException ignored) {
            }
            try {
                candidates.add(location.sourceName());
            } catch (AbsentInformationException ignored) {
            }
            for (Path path : sourceCandidates(sourcePaths, candidates, location.declaringType().name())) {
                if (path.isAbsolute() && Files.isRegularFile(path)) {
                    return path;
                }
                if (Files.isRegularFile(path)) {
                    return path;
                }
            }
            throw new RpcException("source_not_found", "source file not found for " + location.declaringType().name());
        }

        private String threadLabel(ThreadReference thread) {
            return Long.toString(thread.uniqueID()) + " " + thread.name();
        }

        private String objectLabel(ObjectReference object) {
            return object.referenceType().name() + "@" + object.uniqueID();
        }
    }

    static List<Path> sourceCandidates(List<String> roots, List<String> locationCandidates, String className) {
        List<String> candidates = new ArrayList<>();
        for (String candidate : locationCandidates) {
            if (candidate != null && !candidate.isEmpty()) {
                candidates.add(candidate);
            }
        }
        String sourceName = candidates.isEmpty()
                ? null
                : Paths.get(candidates.get(candidates.size() - 1)).getFileName().toString();
        String packagePath = sourcePathFromClassName(className, sourceName);
        if (packagePath != null && !candidates.contains(packagePath)) {
            candidates.add(packagePath);
        }

        Set<Path> out = new LinkedHashSet<>();
        for (String candidate : candidates) {
            Path candidatePath = Paths.get(candidate);
            out.add(candidatePath);
            for (String root : roots) {
                if (root == null || root.isEmpty()) {
                    continue;
                }
                Path rootPath = Paths.get(root);
                out.add(rootPath.resolve(candidate));
                out.add(rootPath.resolve("src").resolve("main").resolve("java").resolve(candidate));
                out.add(rootPath.resolve("src").resolve("test").resolve("java").resolve(candidate));
                out.add(rootPath.resolve("src").resolve("main").resolve("kotlin").resolve(candidate));
                out.add(rootPath.resolve("src").resolve("test").resolve("kotlin").resolve(candidate));
            }
        }
        return new ArrayList<>(out);
    }

    private static String sourcePathFromClassName(String className, String sourceName) {
        if (className == null || className.isEmpty()) {
            return null;
        }
        int nested = className.indexOf('$');
        String outerClass = nested >= 0 ? className.substring(0, nested) : className;
        int dot = outerClass.lastIndexOf('.');
        String simple = dot >= 0 ? outerClass.substring(dot + 1) : outerClass;
        String fileName = sourceName == null || sourceName.isEmpty() ? simple + ".java" : sourceName;
        String packageName = dot >= 0 ? outerClass.substring(0, dot) : "";
        if (packageName.isEmpty()) {
            return fileName;
        }
        return packageName.replace('.', File.separatorChar) + File.separator + fileName;
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

    private static final class StopRecord {
        final Map<String, Object> payload;
        final EventSet eventSet;
        final boolean deliverToWaitingRequest;

        StopRecord(Map<String, Object> payload, EventSet eventSet, boolean deliverToWaitingRequest) {
            this.payload = payload;
            this.eventSet = eventSet;
            this.deliverToWaitingRequest = deliverToWaitingRequest;
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

        String spec() {
            return className + ":" + line;
        }

        boolean matches(String spec) {
            return spec().equals(spec);
        }
    }

    private static final class ActiveBreakpoint {
        final String spec;
        final String className;
        final int line;
        final EventRequest request;

        ActiveBreakpoint(String spec, String className, int line, EventRequest request) {
            this.spec = spec;
            this.className = className;
            this.line = line;
            this.request = request;
        }

        boolean matches(String other) {
            return spec.equals(other) || (className + ":" + line).equals(other);
        }
    }

    private static final class PendingWatchpoint {
        final FieldSpec field;
        String mode;
        ClassPrepareRequest prepareRequest;

        PendingWatchpoint(FieldSpec field, String mode) {
            this.field = field;
            this.mode = mode;
        }

        boolean removeMode(FieldSpec other, String otherMode) {
            if (!field.spec.equals(other.spec)) {
                return false;
            }
            if ("all".equals(otherMode) || mode.equals(otherMode)) {
                return true;
            }
            if ("all".equals(mode)) {
                if ("modification".equals(otherMode)) {
                    mode = "access";
                } else if ("access".equals(otherMode)) {
                    mode = "modification";
                }
            }
            return false;
        }
    }

    private static final class ActiveWatchpoint {
        final FieldSpec field;
        final String mode;
        final EventRequest request;

        ActiveWatchpoint(FieldSpec field, String mode, EventRequest request) {
            this.field = field;
            this.mode = mode;
            this.request = request;
        }

        boolean matches(FieldSpec other, String otherMode) {
            return field.spec.equals(other.spec) && (mode.equals(otherMode) || "all".equals(otherMode));
        }
    }

    private static final class ActiveMethodEvent {
        final MethodSpec spec;
        final String eventKind;
        final EventRequest request;

        ActiveMethodEvent(MethodSpec spec, String eventKind, EventRequest request) {
            this.spec = spec;
            this.eventKind = eventKind;
            this.request = request;
        }

        boolean matches(String clearSpec) {
            return spec.matchesClearSpec(clearSpec);
        }
    }

    private static final class ActiveExceptionRequest {
        final String exceptionName;
        final String mode;
        final ExceptionRequest request;

        ActiveExceptionRequest(String exceptionName, String mode, ExceptionRequest request) {
            this.exceptionName = exceptionName;
            this.mode = mode;
            this.request = request;
        }

        boolean matches(String otherExceptionName, String otherMode) {
            return exceptionName.equals(otherExceptionName) && ("all".equals(otherMode) || mode.equals(otherMode));
        }

        boolean matchesException(String thrownName) {
            return exceptionName.equals(thrownName) || thrownName.endsWith("." + exceptionName);
        }
    }

    static final class MethodSpec {
        final String className;
        final String methodName;
        final List<String> argumentTypeNames;
        final String eventKind;
        final String suspend;

        private MethodSpec(
                String className,
                String methodName,
                List<String> argumentTypeNames,
                String eventKind,
                String suspend
        ) {
            this.className = className;
            this.methodName = methodName;
            this.argumentTypeNames = argumentTypeNames;
            this.eventKind = eventKind;
            this.suspend = suspend;
        }

        static MethodSpec from(String className, String methodName, String args, String eventKind, String suspend) {
            if (!"entry".equals(eventKind) && !"exit".equals(eventKind) && !"both".equals(eventKind)) {
                throw new IllegalArgumentException("event must be entry, exit, or both: " + eventKind);
            }
            if (!"all".equals(suspend) && !"thread".equals(suspend)) {
                throw new IllegalArgumentException("suspend must be all or thread: " + suspend);
            }
            List<String> parsedArgs = null;
            if (args != null && args.trim().length() > 0) {
                parsedArgs = new ArrayList<>();
                String[] parts = args.split(",");
                for (String part : parts) {
                    parsedArgs.add(part.trim());
                }
            }
            return new MethodSpec(className, methodName, parsedArgs, eventKind, suspend);
        }

        boolean matches(Method method) {
            if (!methodName.equals(method.name())) {
                return false;
            }
            if (argumentTypeNames == null) {
                return true;
            }
            return argumentTypeNames.equals(method.argumentTypeNames());
        }

        boolean matches(String methodName, String signature) {
            if (!this.methodName.equals(methodName)) {
                return false;
            }
            if (argumentTypeNames == null) {
                return true;
            }
            StringBuilder descriptor = new StringBuilder("(");
            for (String arg : argumentTypeNames) {
                descriptor.append(typeDescriptor(arg));
            }
            descriptor.append(')');
            return signature.startsWith(descriptor.toString());
        }

        boolean matchesClearSpec(String clearSpec) {
            return display().equals(clearSpec) || (className + "." + methodName).equals(clearSpec);
        }

        String display() {
            if (argumentTypeNames == null) {
                return className + "." + methodName;
            }
            StringBuilder out = new StringBuilder(className)
                    .append('.')
                    .append(methodName)
                    .append('(');
            for (int i = 0; i < argumentTypeNames.size(); i++) {
                if (i > 0) {
                    out.append(',');
                }
                out.append(argumentTypeNames.get(i));
            }
            return out.append(')').toString();
        }

        private static String typeDescriptor(String typeName) {
            String type = typeName.trim();
            int dimensions = 0;
            while (type.endsWith("[]")) {
                dimensions++;
                type = type.substring(0, type.length() - 2);
            }
            String base;
            if ("boolean".equals(type)) {
                base = "Z";
            } else if ("byte".equals(type)) {
                base = "B";
            } else if ("char".equals(type)) {
                base = "C";
            } else if ("short".equals(type)) {
                base = "S";
            } else if ("int".equals(type)) {
                base = "I";
            } else if ("long".equals(type)) {
                base = "J";
            } else if ("float".equals(type)) {
                base = "F";
            } else if ("double".equals(type)) {
                base = "D";
            } else {
                base = "L" + type.replace('.', '/') + ";";
            }
            StringBuilder out = new StringBuilder();
            for (int i = 0; i < dimensions; i++) {
                out.append('[');
            }
            return out.append(base).toString();
        }
    }

    private static final class FieldSpec {
        final String spec;
        final String className;
        final String fieldName;

        FieldSpec(String spec, String className, String fieldName) {
            this.spec = spec;
            this.className = className;
            this.fieldName = fieldName;
        }
    }

    private static final class EvaluationContext {
        final ThreadReference thread;
        final StackFrame frame;

        EvaluationContext(ThreadReference thread, StackFrame frame) {
            this.thread = thread;
            this.frame = frame;
        }
    }

    private static FieldSpec parseFieldSpec(String spec) throws RpcException {
        int dot = spec.lastIndexOf('.');
        if (dot <= 0 || dot == spec.length() - 1) {
            throw new RpcException("bad_field_spec", "field spec must be Class.field: " + spec);
        }
        return new FieldSpec(spec, spec.substring(0, dot), spec.substring(dot + 1));
    }

    private static void validateWatchMode(String mode) throws RpcException {
        if (!"access".equals(mode) && !"modification".equals(mode) && !"all".equals(mode)) {
            throw new RpcException("bad_watch_mode", "watch mode must be access, modification, or all: " + mode);
        }
    }
}
