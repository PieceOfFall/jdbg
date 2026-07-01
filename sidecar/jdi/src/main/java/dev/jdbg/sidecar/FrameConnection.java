package dev.jdbg.sidecar;

import java.io.DataInputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.Map;

final class FrameConnection implements AutoCloseable {
    private static final int MAX_FRAME_SIZE = 8 * 1024 * 1024;

    private final Socket socket;
    private final DataInputStream in;
    private final OutputStream out;

    FrameConnection(Socket socket) throws IOException {
        this.socket = socket;
        this.in = new DataInputStream(socket.getInputStream());
        this.out = socket.getOutputStream();
    }

    Map<String, Object> readMessage() throws IOException {
        int len = in.readInt();
        if (len < 0 || len > MAX_FRAME_SIZE) {
            throw new IOException("invalid frame length " + len);
        }
        byte[] body = new byte[len];
        in.readFully(body);
        return Json.asObject(Json.parse(new String(body, StandardCharsets.UTF_8)), "message");
    }

    synchronized void writeMessage(Map<String, Object> message) throws IOException {
        byte[] body = Json.stringify(message).getBytes(StandardCharsets.UTF_8);
        if (body.length > MAX_FRAME_SIZE) {
            throw new IOException("frame too large: " + body.length);
        }
        out.write((body.length >>> 24) & 0xff);
        out.write((body.length >>> 16) & 0xff);
        out.write((body.length >>> 8) & 0xff);
        out.write(body.length & 0xff);
        out.write(body);
        out.flush();
    }

    @Override
    public void close() throws IOException {
        socket.close();
    }
}
