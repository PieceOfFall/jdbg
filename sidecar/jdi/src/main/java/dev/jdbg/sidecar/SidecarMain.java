package dev.jdbg.sidecar;

import java.util.Map;

public final class SidecarMain {
    static final String VERSION = "1.0.0";

    private SidecarMain() {
    }

    public static void main(String[] args) {
        try {
            Config config = Config.parse(args);
            try (FrameConnection connection = FrameConnection.connect(config)) {
                handshake(connection, config);
                serve(connection);
            }
        } catch (Exception e) {
            System.err.println("jdbg JDI sidecar failed: " + e.getMessage());
            System.exit(1);
        }
    }

    private static void handshake(FrameConnection connection, Config config) throws Exception {
        connection.writeMessage(Json.object(
                "type", "request",
                "id", "handshake",
                "method", "handshake",
                "params", Json.object(
                        "protocol_version", config.protocolVersion,
                        "server_version", VERSION,
                        "token", config.token
                )
        ));
        Map<String, Object> response = connection.readMessage();
        if (response.get("error") != null) {
            throw new IllegalStateException("handshake rejected");
        }
    }

    private static void serve(FrameConnection connection) throws Exception {
        JdiService service = new JdiService(connection);
        boolean running = true;
        while (running) {
            Map<String, Object> message = connection.readMessage();
            if (!"request".equals(message.get("type"))) {
                continue;
            }
            String id = Json.string(message, "id");
            String method = Json.string(message, "method");
            Map<String, Object> params = Json.asObject(message.get("params"), "params");
            if (service.isExecutableEvaluation(method)) {
                Thread worker = new Thread(
                        () -> respond(connection, service, id, method, params),
                        "jdbg-jdi-eval-" + id
                );
                // An invocation may be permanently blocked in the target. It must not
                // keep the sidecar process alive after the client has shut it down.
                worker.setDaemon(true);
                worker.start();
                continue;
            }
            respond(connection, service, id, method, params);
            if ("shutdown".equals(method)) {
                running = false;
            }
        }
    }

    /**
     * Send one response without letting a slow target invocation block the request reader.
     * FrameConnection serializes writes, so async evaluation responses and debugger events
     * cannot interleave their length-prefixed frames.
     */
    private static void respond(
            FrameConnection connection,
            JdiService service,
            String id,
            String method,
            Map<String, Object> params
    ) {
            try {
                Object result = service.call(method, params);
                connection.writeMessage(Json.object(
                        "type", "response",
                        "id", id,
                        "result", result
                ));
            } catch (RpcException e) {
                writeError(connection, id, e.code, e.getMessage());
            } catch (Exception e) {
                writeError(connection, id, "internal_error", e.getMessage());
            } catch (Throwable e) {
                // Errors (e.g. NoClassDefFoundError for com.sun.jdi when a JDK 8 sidecar
                // is launched without tools.jar) are not Exceptions, so without this catch
                // they would escape serve()/main() and crash the process silently -- which
                // the client only sees as a request timeout. Report the real cause instead.
                writeError(connection, id, "internal_error", e.toString());
            }
    }

    private static void writeError(FrameConnection connection, String id, String code, String message) {
        try {
            connection.writeMessage(Json.object(
                    "type", "response",
                    "id", id,
                    "error", Json.object("code", code, "message", message)
            ));
        } catch (Exception ignored) {
            // The client has already disconnected; there is no response channel left to recover.
            }
        }

    static final class Config {
        final String transport;
        final String endpoint;
        final String token;
        final int protocolVersion;

        Config(String transport, String endpoint, String token, int protocolVersion) {
            this.transport = transport;
            this.endpoint = endpoint;
            this.token = token;
            this.protocolVersion = protocolVersion;
        }

        static Config parse(String[] args) {
            String transport = null;
            String endpoint = null;
            String token = null;
            int protocolVersion = 1;
            for (int i = 0; i < args.length; i++) {
                switch (args[i]) {
                    case "--transport":
                        transport = args[++i];
                        break;
                    case "--endpoint":
                        endpoint = args[++i];
                        break;
                    case "--token":
                        token = args[++i];
                        break;
                    case "--protocol-version":
                        protocolVersion = Integer.parseInt(args[++i]);
                        break;
                    default:
                        throw new IllegalArgumentException("unknown argument: " + args[i]);
                }
            }
            if (transport == null || transport.isEmpty()) {
                throw new IllegalArgumentException("--transport is required");
            }
            if (endpoint == null || endpoint.isEmpty()) {
                throw new IllegalArgumentException("--endpoint is required");
            }
            if (token == null || token.isEmpty()) {
                throw new IllegalArgumentException("--token is required");
            }
            if (!"named-pipe".equals(transport) && !"unix-domain-socket".equals(transport)) {
                throw new IllegalArgumentException("unsupported --transport: " + transport);
            }
            return new Config(transport, endpoint, token, protocolVersion);
        }
    }
}
