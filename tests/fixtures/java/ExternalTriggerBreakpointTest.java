import java.io.InputStream;
import java.net.ServerSocket;
import java.net.Socket;

public class ExternalTriggerBreakpointTest {
    static volatile boolean alive = true;

    public static void main(String[] args) throws Exception {
        int port = Integer.parseInt(args[0]);
        ServerSocket server = new ServerSocket(port);
        Thread worker = new Thread(new Runnable() {
            @Override
            public void run() {
                try {
                    Socket socket = server.accept();
                    InputStream in = socket.getInputStream();
                    while (in.read() != -1) {
                    }
                    socket.close();
                    new Handler().handleRequest();
                } catch (Exception ignored) {
                }
            }
        }, "external-trigger-worker");
        worker.setDaemon(true);
        worker.start();
        while (alive) {
            Thread.sleep(1000);
        }
    }

    static final class Handler {
        void handleRequest() throws Exception {
            int marker = 42; // EXTERNAL_TRIGGER_BREAKPOINT
            System.out.println("marker=" + marker);
            Thread.sleep(300000);
        }
    }
}
