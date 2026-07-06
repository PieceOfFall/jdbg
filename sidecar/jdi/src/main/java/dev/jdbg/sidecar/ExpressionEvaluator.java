package dev.jdbg.sidecar;

import com.github.javaparser.StaticJavaParser;
import com.github.javaparser.ast.NodeList;
import com.github.javaparser.ast.expr.ArrayAccessExpr;
import com.github.javaparser.ast.expr.BinaryExpr;
import com.github.javaparser.ast.expr.BooleanLiteralExpr;
import com.github.javaparser.ast.expr.CastExpr;
import com.github.javaparser.ast.expr.CharLiteralExpr;
import com.github.javaparser.ast.expr.DoubleLiteralExpr;
import com.github.javaparser.ast.expr.EnclosedExpr;
import com.github.javaparser.ast.expr.Expression;
import com.github.javaparser.ast.expr.FieldAccessExpr;
import com.github.javaparser.ast.expr.IntegerLiteralExpr;
import com.github.javaparser.ast.expr.LiteralStringValueExpr;
import com.github.javaparser.ast.expr.LongLiteralExpr;
import com.github.javaparser.ast.expr.MethodCallExpr;
import com.github.javaparser.ast.expr.NameExpr;
import com.github.javaparser.ast.expr.NullLiteralExpr;
import com.github.javaparser.ast.expr.StringLiteralExpr;
import com.github.javaparser.ast.expr.ThisExpr;
import com.github.javaparser.ast.expr.UnaryExpr;
import com.github.javaparser.ast.nodeTypes.NodeWithSimpleName;
import com.sun.jdi.AbsentInformationException;
import com.sun.jdi.ArrayReference;
import com.sun.jdi.ArrayType;
import com.sun.jdi.BooleanValue;
import com.sun.jdi.CharValue;
import com.sun.jdi.ClassNotLoadedException;
import com.sun.jdi.ClassType;
import com.sun.jdi.DoubleValue;
import com.sun.jdi.Field;
import com.sun.jdi.FloatValue;
import com.sun.jdi.IncompatibleThreadStateException;
import com.sun.jdi.IntegerValue;
import com.sun.jdi.InterfaceType;
import com.sun.jdi.InvalidTypeException;
import com.sun.jdi.InvocationException;
import com.sun.jdi.LocalVariable;
import com.sun.jdi.LongValue;
import com.sun.jdi.Method;
import com.sun.jdi.ObjectReference;
import com.sun.jdi.PrimitiveValue;
import com.sun.jdi.ReferenceType;
import com.sun.jdi.ShortValue;
import com.sun.jdi.StackFrame;
import com.sun.jdi.StringReference;
import com.sun.jdi.ThreadReference;
import com.sun.jdi.Type;
import com.sun.jdi.Value;
import com.sun.jdi.VirtualMachine;

import java.util.ArrayList;
import java.util.List;

final class ExpressionEvaluator {
    private final VirtualMachine vm;
    private final ThreadReference thread;
    private final List<LocalSnapshot> locals = new ArrayList<>();
    private final ObjectReference thisObject;
    private final boolean localsUnavailable;
    private final boolean allowInvoke;

    ExpressionEvaluator(VirtualMachine vm, ThreadReference thread, StackFrame frame) {
        this(vm, thread, frame, true);
    }

    /// When {@code allowInvoke} is false the evaluator runs in read-only mode:
    /// names, fields (instance and static), array access and literals resolve as
    /// usual, but any method call is rejected with {@code method_invocation_not_allowed}.
    /// This backs {@code inspect}, which must never invoke target code (getters).
    ExpressionEvaluator(VirtualMachine vm, ThreadReference thread, StackFrame frame, boolean allowInvoke) {
        this.vm = vm;
        this.thread = thread;
        this.thisObject = frame.thisObject();
        this.allowInvoke = allowInvoke;
        boolean unavailable = false;
        try {
            for (LocalVariable variable : frame.visibleVariables()) {
                locals.add(new LocalSnapshot(variable.name(), variable, frame.getValue(variable)));
            }
        } catch (AbsentInformationException e) {
            unavailable = true;
        }
        this.localsUnavailable = unavailable;
    }

    Value evaluate(String source) throws RpcException {
        Expression expr = parseExpression(source);
        return valueOf(evaluate(expr));
    }

    Value setValue(String lvalueSource, String valueSource) throws RpcException {
        AssignmentTarget target = assignmentTarget(parseExpression(lvalueSource));
        Value value = evaluate(valueSource);
        target.set(coerce(value, target.type()));
        return target.get();
    }

    Value forceReturn(String valueSource) throws RpcException {
        Type returnType = returnType();
        if ("void".equals(returnType.name())) {
            throw new RpcException("unsupported_force_return", "void force return is not supported yet");
        }
        Value value = coerce(evaluate(valueSource), returnType);
        try {
            thread.forceEarlyReturn(value);
            return value;
        } catch (InvalidTypeException e) {
            throw new RpcException("type_mismatch", "return value is not assignable to " + returnType.name(), e);
        } catch (ClassNotLoadedException e) {
            throw new RpcException("class_not_loaded", "return type is not loaded: " + e.className(), e);
        } catch (IncompatibleThreadStateException e) {
            throw new RpcException("thread_not_suspended", "current thread is not suspended", e);
        } catch (UnsupportedOperationException e) {
            throw new RpcException("unsupported_force_return", "force return is not supported by this target VM", e);
        }
    }

    private Type returnType() throws RpcException {
        try {
            return currentFrame().location().method().returnType();
        } catch (ClassNotLoadedException e) {
            throw new RpcException("class_not_loaded", "return type is not loaded: " + e.className(), e);
        }
    }

    private StackFrame currentFrame() throws RpcException {
        try {
            if (thread.frameCount() == 0) {
                throw new RpcException("empty_stack", "current thread has no stack frames");
            }
            return thread.frame(0);
        } catch (IncompatibleThreadStateException e) {
            throw new RpcException("thread_not_suspended", "current thread is not suspended", e);
        }
    }

    private Expression parseExpression(String source) throws RpcException {
        if (source == null || source.trim().isEmpty()) {
            throw new RpcException("bad_expression", "empty expression");
        }
        try {
            return StaticJavaParser.parseExpression(source);
        } catch (RuntimeException e) {
            throw new RpcException("bad_expression", "invalid Java expression: " + source, e);
        }
    }

    private Resolved evaluate(Expression expr) throws RpcException {
        if (expr instanceof EnclosedExpr) {
            return evaluate(((EnclosedExpr) expr).getInner());
        }
        if (expr instanceof ThisExpr) {
            return Resolved.value(thisObject);
        }
        if (expr instanceof NameExpr) {
            return resolveName(((NameExpr) expr).getNameAsString());
        }
        if (expr instanceof FieldAccessExpr) {
            return resolveFieldAccess((FieldAccessExpr) expr);
        }
        if (expr instanceof ArrayAccessExpr) {
            return resolveArrayAccess((ArrayAccessExpr) expr);
        }
        if (expr instanceof MethodCallExpr) {
            if (!allowInvoke) {
                throw new RpcException(
                        "method_invocation_not_allowed",
                        "inspect does not invoke methods (getters may have side effects); use print/eval for '"
                                + expr + "'");
            }
            return Resolved.value(invoke((MethodCallExpr) expr));
        }
        if (expr instanceof CastExpr) {
            CastExpr cast = (CastExpr) expr;
            return Resolved.value(coerceToTypeName(valueOf(evaluate(cast.getExpression())), cast.getType().asString()));
        }
        if (expr instanceof UnaryExpr) {
            return Resolved.value(evaluateUnary((UnaryExpr) expr));
        }
        if (expr instanceof BinaryExpr) {
            return Resolved.value(evaluateBinary((BinaryExpr) expr));
        }
        if (expr instanceof NullLiteralExpr) {
            return Resolved.value(null);
        }
        if (expr instanceof BooleanLiteralExpr) {
            return Resolved.value(vm.mirrorOf(((BooleanLiteralExpr) expr).getValue()));
        }
        if (expr instanceof CharLiteralExpr) {
            return Resolved.value(vm.mirrorOf(((CharLiteralExpr) expr).asChar()));
        }
        if (expr instanceof StringLiteralExpr) {
            return Resolved.value(vm.mirrorOf(((StringLiteralExpr) expr).asString()));
        }
        if (expr instanceof IntegerLiteralExpr) {
            return Resolved.value(vm.mirrorOf(parseIntegerLiteral(((IntegerLiteralExpr) expr).getValue())));
        }
        if (expr instanceof LongLiteralExpr) {
            return Resolved.value(vm.mirrorOf(parseLongLiteral(((LongLiteralExpr) expr).getValue())));
        }
        if (expr instanceof DoubleLiteralExpr) {
            return Resolved.value(doubleLiteral((DoubleLiteralExpr) expr));
        }
        if (expr instanceof LiteralStringValueExpr) {
            return Resolved.value(vm.mirrorOf(((LiteralStringValueExpr) expr).getValue()));
        }
        throw new RpcException("unsupported_expression", "unsupported expression: " + expr);
    }

    private Resolved resolveName(String name) throws RpcException {
        try {
            if ("this".equals(name)) {
                return Resolved.value(thisObject);
            }
            for (LocalSnapshot local : locals) {
                if (local.name.equals(name)) {
                    return Resolved.value(local.value);
                }
            }
        } catch (RuntimeException e) {
            throw e;
        }
        if (localsUnavailable) {
            throw new RpcException("locals_unavailable", "local variable information unavailable; compile with javac -g");
        }

        ReferenceType type = findType(name, false);
        if (type != null) {
            return Resolved.type(type);
        }
        throw new RpcException("name_not_found", "name not found in current frame: " + name);
    }

    private Resolved resolveFieldAccess(FieldAccessExpr expr) throws RpcException {
        ReferenceType fullType = findType(expr.toString(), false);
        if (fullType != null) {
            return Resolved.type(fullType);
        }

        Resolved scope = evaluate(expr.getScope());
        String name = expr.getNameAsString();
        if (scope.isType()) {
            ReferenceType nested = findType(scope.type.name() + "." + name, false);
            if (nested == null) {
                nested = findType(scope.type.name() + "$" + name, false);
            }
            if (nested != null) {
                return Resolved.type(nested);
            }
            Field field = findField(scope.type, name);
            return Resolved.value(scope.type.getValue(field));
        }

        Value base = valueOf(scope);
        if (base instanceof ArrayReference && "length".equals(name)) {
            return Resolved.value(vm.mirrorOf(((ArrayReference) base).length()));
        }
        if (!(base instanceof ObjectReference)) {
            throw new RpcException("not_object", "cannot read field '" + name + "' from non-object value");
        }
        ObjectReference object = (ObjectReference) base;
        Field field = findField(object.referenceType(), name);
        if (field.isStatic()) {
            return Resolved.value(object.referenceType().getValue(field));
        }
        return Resolved.value(object.getValue(field));
    }

    private Resolved resolveArrayAccess(ArrayAccessExpr expr) throws RpcException {
        Value base = valueOf(evaluate(expr.getName()));
        if (!(base instanceof ArrayReference)) {
            throw new RpcException("not_array", "array access target is not an array: " + expr.getName());
        }
        int index = intValue(valueOf(evaluate(expr.getIndex())), "array index");
        ArrayReference array = (ArrayReference) base;
        if (index < 0 || index >= array.length()) {
            throw new RpcException("index_out_of_bounds", "array index out of bounds: " + index);
        }
        return Resolved.value(array.getValue(index));
    }

    private Value invoke(MethodCallExpr expr) throws RpcException {
        List<Value> args = evaluateArguments(expr.getArguments());
        String name = expr.getNameAsString();
        Resolved receiver = expr.getScope().isPresent()
                ? evaluate(expr.getScope().get())
                : Resolved.value(thisObject);

        if (receiver.isType()) {
            MethodBinding binding = selectMethod(receiver.type, name, args, true);
            try {
                if (receiver.type instanceof ClassType) {
                    return ((ClassType) receiver.type).invokeMethod(
                            thread,
                            binding.method,
                            binding.args,
                            ClassType.INVOKE_SINGLE_THREADED
                    );
                }
                if (receiver.type instanceof InterfaceType) {
                    return ((InterfaceType) receiver.type).invokeMethod(
                            thread,
                            binding.method,
                            binding.args,
                            ClassType.INVOKE_SINGLE_THREADED
                    );
                }
                throw new RpcException("not_invokable", "type cannot be invoked: " + receiver.type.name());
            } catch (InvocationException e) {
                throw methodThrew(name, e);
            } catch (InvalidTypeException e) {
                throw new RpcException("type_mismatch", "method arguments are not assignable for " + name, e);
            } catch (ClassNotLoadedException e) {
                throw new RpcException("class_not_loaded", "method argument type is not loaded: " + e.className(), e);
            } catch (IncompatibleThreadStateException e) {
                throw new RpcException("thread_not_suspended", "current thread is not suspended", e);
            }
        }

        Value value = valueOf(receiver);
        if (!(value instanceof ObjectReference)) {
            throw new RpcException("not_object", "cannot call method '" + name + "' on non-object value");
        }
        ObjectReference object = (ObjectReference) value;
        MethodBinding binding = selectMethod(object.referenceType(), name, args, false);
        try {
            return object.invokeMethod(
                    thread,
                    binding.method,
                    binding.args,
                    ObjectReference.INVOKE_SINGLE_THREADED
            );
        } catch (InvocationException e) {
            throw methodThrew(name, e);
        } catch (InvalidTypeException e) {
            throw new RpcException("type_mismatch", "method arguments are not assignable for " + name, e);
        } catch (ClassNotLoadedException e) {
            throw new RpcException("class_not_loaded", "method argument type is not loaded: " + e.className(), e);
        } catch (IncompatibleThreadStateException e) {
            throw new RpcException("thread_not_suspended", "current thread is not suspended", e);
        }
    }

    private RpcException methodThrew(String method, InvocationException e) {
        ObjectReference exception = e.exception();
        String type = exception == null ? "unknown" : exception.referenceType().name();
        return new RpcException("method_threw", "method threw while evaluating " + method + ": " + type, e);
    }

    private MethodBinding selectMethod(ReferenceType type, String name, List<Value> args, boolean requireStatic)
            throws RpcException {
        List<Method> candidates = type.methodsByName(name);
        if (candidates.isEmpty()) {
            candidates = new ArrayList<>();
            for (Method method : type.allMethods()) {
                if (method.name().equals(name)) {
                    candidates.add(method);
                }
            }
        }

        for (Method method : candidates) {
            if (method.isStatic() != requireStatic || method.argumentTypeNames().size() != args.size()) {
                continue;
            }
            try {
                return new MethodBinding(method, coerceArguments(args, method.argumentTypes()));
            } catch (ClassNotLoadedException e) {
                throw new RpcException("class_not_loaded", "method argument type is not loaded: " + e.className(), e);
            } catch (RpcException ignored) {
            }
        }
        throw new RpcException("method_not_found", "method not found or arguments do not match: " + type.name() + "." + name);
    }

    private List<Value> evaluateArguments(NodeList<Expression> expressions) throws RpcException {
        List<Value> args = new ArrayList<>();
        for (Expression expression : expressions) {
            args.add(valueOf(evaluate(expression)));
        }
        return args;
    }

    private List<Value> coerceArguments(List<Value> args, List<Type> types) throws RpcException {
        List<Value> out = new ArrayList<>();
        for (int i = 0; i < args.size(); i++) {
            out.add(coerce(args.get(i), types.get(i)));
        }
        return out;
    }

    private Value evaluateUnary(UnaryExpr expr) throws RpcException {
        Value value = valueOf(evaluate(expr.getExpression()));
        switch (expr.getOperator()) {
            case PLUS:
                return numericUnary(value, false);
            case MINUS:
                return numericUnary(value, true);
            case LOGICAL_COMPLEMENT:
                return vm.mirrorOf(!booleanValue(value, "logical complement"));
            case BITWISE_COMPLEMENT:
                if (value instanceof LongValue) {
                    return vm.mirrorOf(~((LongValue) value).longValue());
                }
                return vm.mirrorOf(~intValue(value, "bitwise complement"));
            default:
                throw new RpcException("unsupported_expression", "unsupported unary operator: " + expr.getOperator());
        }
    }

    private Value evaluateBinary(BinaryExpr expr) throws RpcException {
        switch (expr.getOperator()) {
            case AND:
                return vm.mirrorOf(booleanValue(valueOf(evaluate(expr.getLeft())), "left operand")
                        && booleanValue(valueOf(evaluate(expr.getRight())), "right operand"));
            case OR:
                return vm.mirrorOf(booleanValue(valueOf(evaluate(expr.getLeft())), "left operand")
                        || booleanValue(valueOf(evaluate(expr.getRight())), "right operand"));
            default:
                break;
        }

        Value left = valueOf(evaluate(expr.getLeft()));
        Value right = valueOf(evaluate(expr.getRight()));
        switch (expr.getOperator()) {
            case PLUS:
                if (isStringLike(left) || isStringLike(right)) {
                    return vm.mirrorOf(stringValue(left) + stringValue(right));
                }
                return numericBinary(left, right, '+');
            case MINUS:
                return numericBinary(left, right, '-');
            case MULTIPLY:
                return numericBinary(left, right, '*');
            case DIVIDE:
                return numericBinary(left, right, '/');
            case REMAINDER:
                return numericBinary(left, right, '%');
            case BINARY_AND:
                return vm.mirrorOf(intValue(left, "left operand") & intValue(right, "right operand"));
            case BINARY_OR:
                return vm.mirrorOf(intValue(left, "left operand") | intValue(right, "right operand"));
            case XOR:
                return vm.mirrorOf(intValue(left, "left operand") ^ intValue(right, "right operand"));
            case EQUALS:
                return vm.mirrorOf(equalsValue(left, right));
            case NOT_EQUALS:
                return vm.mirrorOf(!equalsValue(left, right));
            case LESS:
                return vm.mirrorOf(doubleValue(left, "left operand") < doubleValue(right, "right operand"));
            case LESS_EQUALS:
                return vm.mirrorOf(doubleValue(left, "left operand") <= doubleValue(right, "right operand"));
            case GREATER:
                return vm.mirrorOf(doubleValue(left, "left operand") > doubleValue(right, "right operand"));
            case GREATER_EQUALS:
                return vm.mirrorOf(doubleValue(left, "left operand") >= doubleValue(right, "right operand"));
            default:
                throw new RpcException("unsupported_expression", "unsupported binary operator: " + expr.getOperator());
        }
    }

    private AssignmentTarget assignmentTarget(Expression expr) throws RpcException {
        if (expr instanceof EnclosedExpr) {
            return assignmentTarget(((EnclosedExpr) expr).getInner());
        }
        if (expr instanceof NameExpr) {
            return localTarget(((NameExpr) expr).getNameAsString());
        }
        if (expr instanceof FieldAccessExpr) {
            return fieldTarget((FieldAccessExpr) expr);
        }
        if (expr instanceof ArrayAccessExpr) {
            return arrayTarget((ArrayAccessExpr) expr);
        }
        throw new RpcException("bad_lvalue", "expression is not assignable: " + expr);
    }

    private AssignmentTarget localTarget(String name) throws RpcException {
        for (LocalSnapshot local : locals) {
            if (local.name.equals(name)) {
                return new LocalTarget(local.variable);
            }
        }
        if (localsUnavailable) {
            throw new RpcException("locals_unavailable", "local variable information unavailable; compile with javac -g");
        }
        throw new RpcException("name_not_found", "local variable not found: " + name);
    }

    private AssignmentTarget fieldTarget(FieldAccessExpr expr) throws RpcException {
        Resolved scope = evaluate(expr.getScope());
        String name = expr.getNameAsString();
        if (scope.isType()) {
            Field field = findField(scope.type, name);
            if (!(scope.type instanceof ClassType)) {
                throw new RpcException("bad_lvalue", "static field assignment requires a class type: " + scope.type.name());
            }
            return new StaticFieldTarget((ClassType) scope.type, field);
        }

        Value base = valueOf(scope);
        if (!(base instanceof ObjectReference)) {
            throw new RpcException("not_object", "cannot assign field '" + name + "' on non-object value");
        }
        ObjectReference object = (ObjectReference) base;
        Field field = findField(object.referenceType(), name);
        if (field.isStatic()) {
            if (!(object.referenceType() instanceof ClassType)) {
                throw new RpcException("bad_lvalue", "static field assignment requires a class type: " + object.referenceType().name());
            }
            return new StaticFieldTarget((ClassType) object.referenceType(), field);
        }
        return new ObjectFieldTarget(object, field);
    }

    private AssignmentTarget arrayTarget(ArrayAccessExpr expr) throws RpcException {
        Value base = valueOf(evaluate(expr.getName()));
        if (!(base instanceof ArrayReference)) {
            throw new RpcException("not_array", "array assignment target is not an array: " + expr.getName());
        }
        int index = intValue(valueOf(evaluate(expr.getIndex())), "array index");
        ArrayReference array = (ArrayReference) base;
        if (index < 0 || index >= array.length()) {
            throw new RpcException("index_out_of_bounds", "array index out of bounds: " + index);
        }
        return new ArrayElementTarget(array, index);
    }

    private Field findField(ReferenceType type, String name) throws RpcException {
        for (Field field : type.allFields()) {
            if (field.name().equals(name)) {
                return field;
            }
        }
        throw new RpcException("field_not_found", "field not found: " + type.name() + "." + name);
    }

    private ReferenceType findType(String name, boolean required) throws RpcException {
        List<ReferenceType> exact = vm.classesByName(name);
        if (!exact.isEmpty()) {
            return exact.get(0);
        }

        ReferenceType match = null;
        for (ReferenceType type : vm.allClasses()) {
            if (type.name().equals(name) || type.name().endsWith("." + name) || type.name().endsWith("$" + name)) {
                if (match != null && !match.name().equals(type.name())) {
                    throw new RpcException("ambiguous_type", "type name is ambiguous: " + name);
                }
                match = type;
            }
        }
        if (match == null && required) {
            throw new RpcException("type_not_found", "type not found: " + name);
        }
        return match;
    }

    private Value coerce(Value value, Type target) throws RpcException {
        return coerceToTypeName(value, target.name(), target);
    }

    private Value coerceToTypeName(Value value, String typeName) throws RpcException {
        return coerceToTypeName(value, typeName, findType(typeName, false));
    }

    private Value coerceToTypeName(Value value, String typeName, Type target) throws RpcException {
        if ("boolean".equals(typeName)) {
            return vm.mirrorOf(booleanValue(value, "boolean value"));
        }
        if ("char".equals(typeName)) {
            if (value instanceof CharValue) {
                return vm.mirrorOf(((CharValue) value).charValue());
            }
            return vm.mirrorOf((char) intValue(value, "char value"));
        }
        if ("byte".equals(typeName)) {
            return vm.mirrorOf((byte) intValue(value, "byte value"));
        }
        if ("short".equals(typeName)) {
            return vm.mirrorOf((short) intValue(value, "short value"));
        }
        if ("int".equals(typeName)) {
            return vm.mirrorOf(intValue(value, "int value"));
        }
        if ("long".equals(typeName)) {
            return vm.mirrorOf(longValue(value, "long value"));
        }
        if ("float".equals(typeName)) {
            return vm.mirrorOf((float) doubleValue(value, "float value"));
        }
        if ("double".equals(typeName)) {
            return vm.mirrorOf(doubleValue(value, "double value"));
        }
        if (value == null) {
            return null;
        }
        if ("java.lang.String".equals(typeName) && value instanceof StringReference) {
            return value;
        }
        if (!(value instanceof ObjectReference)) {
            throw new RpcException("type_mismatch", "cannot assign primitive value to " + typeName);
        }
        if (target instanceof ReferenceType && isAssignable((ObjectReference) value, (ReferenceType) target)) {
            return value;
        }
        throw new RpcException("type_mismatch", "value is not assignable to " + typeName);
    }

    private boolean isAssignable(ObjectReference value, ReferenceType target) {
        return isSubtypeOf(value.referenceType(), target.name());
    }

    private boolean isSubtypeOf(ReferenceType type, String expectedName) {
        if (type.name().equals(expectedName) || "java.lang.Object".equals(expectedName)) {
            return true;
        }
        if (type instanceof ClassType) {
            ClassType cls = (ClassType) type;
            for (InterfaceType iface : cls.allInterfaces()) {
                if (interfaceMatches(iface, expectedName)) {
                    return true;
                }
            }
            ClassType superClass = cls.superclass();
            while (superClass != null) {
                if (superClass.name().equals(expectedName)) {
                    return true;
                }
                for (InterfaceType iface : superClass.allInterfaces()) {
                    if (interfaceMatches(iface, expectedName)) {
                        return true;
                    }
                }
                superClass = superClass.superclass();
            }
        }
        if (type instanceof InterfaceType) {
            return interfaceMatches((InterfaceType) type, expectedName);
        }
        return false;
    }

    private boolean interfaceMatches(InterfaceType iface, String expectedName) {
        if (iface.name().equals(expectedName)) {
            return true;
        }
        for (InterfaceType parent : iface.superinterfaces()) {
            if (interfaceMatches(parent, expectedName)) {
                return true;
            }
        }
        return false;
    }

    private Value numericUnary(Value value, boolean negative) throws RpcException {
        if (value instanceof DoubleValue || value instanceof FloatValue) {
            double result = doubleValue(value, "numeric operand");
            return vm.mirrorOf(negative ? -result : result);
        }
        if (value instanceof LongValue) {
            long result = longValue(value, "numeric operand");
            return vm.mirrorOf(negative ? -result : result);
        }
        int result = intValue(value, "numeric operand");
        return vm.mirrorOf(negative ? -result : result);
    }

    private Value numericBinary(Value left, Value right, char operator) throws RpcException {
        if (left instanceof DoubleValue || left instanceof FloatValue || right instanceof DoubleValue || right instanceof FloatValue) {
            double l = doubleValue(left, "left operand");
            double r = doubleValue(right, "right operand");
            switch (operator) {
                case '+':
                    return vm.mirrorOf(l + r);
                case '-':
                    return vm.mirrorOf(l - r);
                case '*':
                    return vm.mirrorOf(l * r);
                case '/':
                    return vm.mirrorOf(l / r);
                case '%':
                    return vm.mirrorOf(l % r);
                default:
                    throw new RpcException("unsupported_expression", "unsupported numeric operator: " + operator);
            }
        }
        if (left instanceof LongValue || right instanceof LongValue) {
            long l = longValue(left, "left operand");
            long r = longValue(right, "right operand");
            switch (operator) {
                case '+':
                    return vm.mirrorOf(l + r);
                case '-':
                    return vm.mirrorOf(l - r);
                case '*':
                    return vm.mirrorOf(l * r);
                case '/':
                    return vm.mirrorOf(l / r);
                case '%':
                    return vm.mirrorOf(l % r);
                default:
                    throw new RpcException("unsupported_expression", "unsupported numeric operator: " + operator);
            }
        }
        int l = intValue(left, "left operand");
        int r = intValue(right, "right operand");
        switch (operator) {
            case '+':
                return vm.mirrorOf(l + r);
            case '-':
                return vm.mirrorOf(l - r);
            case '*':
                return vm.mirrorOf(l * r);
            case '/':
                return vm.mirrorOf(l / r);
            case '%':
                return vm.mirrorOf(l % r);
            default:
                throw new RpcException("unsupported_expression", "unsupported numeric operator: " + operator);
        }
    }

    private boolean equalsValue(Value left, Value right) {
        if (left == null || right == null) {
            return left == right;
        }
        if (left instanceof PrimitiveValue && right instanceof PrimitiveValue) {
            return left.toString().equals(right.toString());
        }
        if (left instanceof ObjectReference && right instanceof ObjectReference) {
            return ((ObjectReference) left).uniqueID() == ((ObjectReference) right).uniqueID();
        }
        return false;
    }

    private boolean isStringLike(Value value) {
        return value instanceof StringReference;
    }

    private String stringValue(Value value) throws RpcException {
        if (value == null) {
            return "null";
        }
        if (value instanceof StringReference) {
            return ((StringReference) value).value();
        }
        if (value instanceof PrimitiveValue) {
            return value.toString();
        }
        return ValueRenderer.display(value);
    }

    private boolean booleanValue(Value value, String label) throws RpcException {
        if (value instanceof BooleanValue) {
            return ((BooleanValue) value).booleanValue();
        }
        throw new RpcException("type_mismatch", label + " must be boolean");
    }

    private int intValue(Value value, String label) throws RpcException {
        if (value instanceof IntegerValue) {
            return ((IntegerValue) value).intValue();
        }
        if (value instanceof ShortValue) {
            return ((ShortValue) value).shortValue();
        }
        if (value instanceof CharValue) {
            return ((CharValue) value).charValue();
        }
        if (value instanceof PrimitiveValue) {
            long numeric = longValue(value, label);
            if (numeric >= Integer.MIN_VALUE && numeric <= Integer.MAX_VALUE) {
                return (int) numeric;
            }
        }
        throw new RpcException("type_mismatch", label + " must be an int-compatible value");
    }

    private long longValue(Value value, String label) throws RpcException {
        if (value instanceof LongValue) {
            return ((LongValue) value).longValue();
        }
        if (value instanceof IntegerValue) {
            return ((IntegerValue) value).intValue();
        }
        if (value instanceof ShortValue) {
            return ((ShortValue) value).shortValue();
        }
        if (value instanceof CharValue) {
            return ((CharValue) value).charValue();
        }
        throw new RpcException("type_mismatch", label + " must be a long-compatible value");
    }

    private double doubleValue(Value value, String label) throws RpcException {
        if (value instanceof DoubleValue) {
            return ((DoubleValue) value).doubleValue();
        }
        if (value instanceof FloatValue) {
            return ((FloatValue) value).floatValue();
        }
        if (value instanceof LongValue) {
            return ((LongValue) value).longValue();
        }
        if (value instanceof IntegerValue) {
            return ((IntegerValue) value).intValue();
        }
        if (value instanceof ShortValue) {
            return ((ShortValue) value).shortValue();
        }
        if (value instanceof CharValue) {
            return ((CharValue) value).charValue();
        }
        throw new RpcException("type_mismatch", label + " must be a numeric value");
    }

    private Value doubleLiteral(DoubleLiteralExpr expr) {
        String raw = stripNumericSeparators(expr.getValue());
        if (raw.endsWith("f") || raw.endsWith("F")) {
            return vm.mirrorOf(Float.parseFloat(raw.substring(0, raw.length() - 1)));
        }
        if (raw.endsWith("d") || raw.endsWith("D")) {
            raw = raw.substring(0, raw.length() - 1);
        }
        return vm.mirrorOf(Double.parseDouble(raw));
    }

    private int parseIntegerLiteral(String raw) {
        String value = stripNumericSeparators(raw);
        if (value.startsWith("0b") || value.startsWith("0B")) {
            return Integer.parseUnsignedInt(value.substring(2), 2);
        }
        return Integer.decode(value);
    }

    private long parseLongLiteral(String raw) {
        String value = stripNumericSeparators(raw);
        if (value.endsWith("l") || value.endsWith("L")) {
            value = value.substring(0, value.length() - 1);
        }
        if (value.startsWith("0b") || value.startsWith("0B")) {
            return Long.parseUnsignedLong(value.substring(2), 2);
        }
        return Long.decode(value);
    }

    private String stripNumericSeparators(String raw) {
        return raw.replace("_", "");
    }

    private Value valueOf(Resolved resolved) throws RpcException {
        if (resolved.isType()) {
            throw new RpcException("type_mismatch", "type name is not a value: " + resolved.type.name());
        }
        return resolved.value;
    }

    private static final class Resolved {
        final Value value;
        final ReferenceType type;

        private Resolved(Value value, ReferenceType type) {
            this.value = value;
            this.type = type;
        }

        static Resolved value(Value value) {
            return new Resolved(value, null);
        }

        static Resolved type(ReferenceType type) {
            return new Resolved(null, type);
        }

        boolean isType() {
            return type != null;
        }
    }

    private static final class MethodBinding {
        final Method method;
        final List<Value> args;

        MethodBinding(Method method, List<Value> args) {
            this.method = method;
            this.args = args;
        }
    }

    private static final class LocalSnapshot {
        final String name;
        final LocalVariable variable;
        final Value value;

        LocalSnapshot(String name, LocalVariable variable, Value value) {
            this.name = name;
            this.variable = variable;
            this.value = value;
        }
    }

    private interface AssignmentTarget {
        Type type() throws RpcException;

        Value get() throws RpcException;

        void set(Value value) throws RpcException;
    }

    private final class LocalTarget implements AssignmentTarget {
        private final LocalVariable variable;

        LocalTarget(LocalVariable variable) {
            this.variable = variable;
        }

        @Override
        public Type type() throws RpcException {
            try {
                return variable.type();
            } catch (ClassNotLoadedException e) {
                throw new RpcException("class_not_loaded", "local type is not loaded: " + e.className(), e);
            }
        }

        @Override
        public Value get() throws RpcException {
            return currentFrame().getValue(variable);
        }

        @Override
        public void set(Value value) throws RpcException {
            try {
                currentFrame().setValue(variable, value);
            } catch (InvalidTypeException e) {
                throw new RpcException("type_mismatch", "value is not assignable to local " + variable.name(), e);
            } catch (ClassNotLoadedException e) {
                throw new RpcException("class_not_loaded", "local type is not loaded: " + e.className(), e);
            }
        }
    }

    private final class ObjectFieldTarget implements AssignmentTarget {
        private final ObjectReference object;
        private final Field field;

        ObjectFieldTarget(ObjectReference object, Field field) {
            this.object = object;
            this.field = field;
        }

        @Override
        public Type type() throws RpcException {
            try {
                return field.type();
            } catch (ClassNotLoadedException e) {
                throw new RpcException("class_not_loaded", "field type is not loaded: " + e.className(), e);
            }
        }

        @Override
        public Value get() {
            return object.getValue(field);
        }

        @Override
        public void set(Value value) throws RpcException {
            try {
                object.setValue(field, value);
            } catch (InvalidTypeException e) {
                throw new RpcException("type_mismatch", "value is not assignable to field " + field.name(), e);
            } catch (ClassNotLoadedException e) {
                throw new RpcException("class_not_loaded", "field type is not loaded: " + e.className(), e);
            }
        }
    }

    private final class StaticFieldTarget implements AssignmentTarget {
        private final ClassType type;
        private final Field field;

        StaticFieldTarget(ClassType type, Field field) {
            this.type = type;
            this.field = field;
        }

        @Override
        public Type type() throws RpcException {
            try {
                return field.type();
            } catch (ClassNotLoadedException e) {
                throw new RpcException("class_not_loaded", "field type is not loaded: " + e.className(), e);
            }
        }

        @Override
        public Value get() {
            return type.getValue(field);
        }

        @Override
        public void set(Value value) throws RpcException {
            try {
                type.setValue(field, value);
            } catch (InvalidTypeException e) {
                throw new RpcException("type_mismatch", "value is not assignable to field " + field.name(), e);
            } catch (ClassNotLoadedException e) {
                throw new RpcException("class_not_loaded", "field type is not loaded: " + e.className(), e);
            }
        }
    }

    private final class ArrayElementTarget implements AssignmentTarget {
        private final ArrayReference array;
        private final int index;

        ArrayElementTarget(ArrayReference array, int index) {
            this.array = array;
            this.index = index;
        }

        @Override
        public Type type() throws RpcException {
            try {
                return ((ArrayType) array.referenceType()).componentType();
            } catch (ClassNotLoadedException e) {
                throw new RpcException("class_not_loaded", "array component type is not loaded: " + e.className(), e);
            }
        }

        @Override
        public Value get() {
            return array.getValue(index);
        }

        @Override
        public void set(Value value) throws RpcException {
            try {
                array.setValue(index, value);
            } catch (InvalidTypeException e) {
                throw new RpcException("type_mismatch", "value is not assignable to array element", e);
            } catch (ClassNotLoadedException e) {
                throw new RpcException("class_not_loaded", "array component type is not loaded: " + e.className(), e);
            }
        }
    }
}
