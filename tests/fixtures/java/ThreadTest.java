import java.util.concurrent.CountDownLatch;

/**
 * Fixture for thread-suspend-policy tests.
 * Two threads: "worker" hits the breakpoint, "heartbeat" keeps running.
 */
public class ThreadTest {
    static volatile boolean heartbeatAlive = true;
    static volatile int heartbeatCount = 0;

    public static void main(String[] args) throws Exception {
        CountDownLatch latch = new CountDownLatch(1);

        Thread heartbeat = new Thread(() -> {
            latch.countDown();
            while (heartbeatAlive) {
                heartbeatCount++;
                try { Thread.sleep(50); } catch (InterruptedException e) { break; }
            }
        }, "heartbeat");

        Thread worker = new Thread(() -> {
            try { latch.await(); } catch (InterruptedException e) { return; }
            doWork();
        }, "worker");

        heartbeat.setDaemon(true);
        heartbeat.start();
        worker.start();
        worker.join(10000);
        heartbeatAlive = false;
        heartbeat.join(2000);
        System.out.println("done heartbeatCount=" + heartbeatCount);
    }

    static void doWork() {
        int x = 1;
        int y = x + 1;
        int z = y + 1;
        System.out.println("worker result=" + z);
    }
}
