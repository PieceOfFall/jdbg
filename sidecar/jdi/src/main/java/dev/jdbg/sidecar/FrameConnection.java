package dev.jdbg.sidecar;

import java.io.DataInputStream;
import java.io.FileDescriptor;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.reflect.Field;
import java.nio.charset.StandardCharsets;
import java.util.Map;

final class FrameConnection implements AutoCloseable {
    private static final int MAX_FRAME_SIZE = 8 * 1024 * 1024;

    private final AutoCloseable closeable;
    private final DataInputStream in;
    private final OutputStream out;

    FrameConnection(InputStream in, OutputStream out, AutoCloseable closeable) {
        this.closeable = closeable;
        this.in = new DataInputStream(in);
        this.out = out;
    }

    static FrameConnection connect(SidecarMain.Config config) throws Exception {
        if ("named-pipe".equals(config.transport)) {
            FileInputStream in = new FileInputStream(config.endpoint + "-to-sidecar");
            FileOutputStream out = new FileOutputStream(config.endpoint + "-from-sidecar");
            return new FrameConnection(in, out, new CloseBoth(in, out));
        }
        if ("unix-domain-socket".equals(config.transport)) {
            FileDescriptor fd = fileDescriptor(Integer.parseInt(config.endpoint));
            return new FrameConnection(new FileInputStream(fd), new FileOutputStream(fd), new CloseFileDescriptor(fd));
        }
        throw new IllegalArgumentException("unsupported transport: " + config.transport);
    }

    private static FileDescriptor fileDescriptor(int fd) throws Exception {
        FileDescriptor desc = new FileDescriptor();
        Field field = FileDescriptor.class.getDeclaredField("fd");
        field.setAccessible(true);
        field.setInt(desc, fd);
        return desc;
    }

    private static final class CloseFileDescriptor implements AutoCloseable {
        private final FileDescriptor fd;

        CloseFileDescriptor(FileDescriptor fd) {
            this.fd = fd;
        }

        @Override
        public void close() throws IOException {
            new FileInputStream(fd).close();
        }
    }

    private static final class CloseBoth implements AutoCloseable {
        private final AutoCloseable first;
        private final AutoCloseable second;

        CloseBoth(AutoCloseable first, AutoCloseable second) {
            this.first = first;
            this.second = second;
        }

        @Override
        public void close() throws Exception {
            try {
                first.close();
            } finally {
                second.close();
            }
        }
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
    public void close() throws Exception {
        closeable.close();
    }
}
