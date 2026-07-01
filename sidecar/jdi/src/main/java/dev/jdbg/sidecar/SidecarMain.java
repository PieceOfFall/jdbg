package dev.jdbg.sidecar;

import java.net.Socket;
import java.util.Map;

public final class SidecarMain {
    static final String VERSION = "1.0.0";

    private SidecarMain() {
    }

    public static void main(String[] args) {
        try {
            Config config = Config.parse(args);
            try (Socket socket = new Socket("127.0.0.1", config.port);
                 FrameConnection connection = new FrameConnection(socket)) {
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
            try {
                Object result = service.call(method, params);
                connection.writeMessage(Json.object(
                        "type", "response",
                        "id", id,
                        "result", result
                ));
                if ("shutdown".equals(method)) {
                    running = false;
                }
            } catch (RpcException e) {
                connection.writeMessage(Json.object(
                        "type", "response",
                        "id", id,
                        "error", Json.object("code", e.code, "message", e.getMessage())
                ));
            } catch (Exception e) {
                connection.writeMessage(Json.object(
                        "type", "response",
                        "id", id,
                        "error", Json.object("code", "internal_error", "message", e.getMessage())
                ));
            }
        }
    }

    private static final class Config {
        final int port;
        final String token;
        final int protocolVersion;

        Config(int port, String token, int protocolVersion) {
            this.port = port;
            this.token = token;
            this.protocolVersion = protocolVersion;
        }

        static Config parse(String[] args) {
            Integer port = null;
            String token = null;
            int protocolVersion = 1;
            for (int i = 0; i < args.length; i++) {
                switch (args[i]) {
                    case "--port":
                        port = Integer.parseInt(args[++i]);
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
            if (port == null) {
                throw new IllegalArgumentException("--port is required");
            }
            if (token == null || token.isEmpty()) {
                throw new IllegalArgumentException("--token is required");
            }
            return new Config(port, token, protocolVersion);
        }
    }
}
