import java.io.File;
import java.util.concurrent.CountDownLatch;

public class ConcurrentThreadStopTest {
    public static void main(String[] args) throws Exception {
        final File gate = new File(args[0]);
        final CountDownLatch ready = new CountDownLatch(2);
        final CountDownLatch fire = new CountDownLatch(1);
        startWorker("first-worker", ready, fire);
        startWorker("second-worker", ready, fire);
        ready.await();
        while (!gate.exists()) {
            Thread.sleep(20);
        }
        fire.countDown();
        Thread.sleep(300000);
    }

    private static void startWorker(final String worker, final CountDownLatch ready, final CountDownLatch fire) {
        new Thread(new Runnable() {
            @Override
            public void run() {
                try {
                    ready.countDown();
                    fire.await();
                    hit(worker);
                    Thread.sleep(300000);
                } catch (InterruptedException ignored) {
                }
            }
        }, worker).start();
    }

    private static void hit(String worker) {
        int marker = worker.length(); // CONCURRENT_THREAD_STOP
        System.out.println(marker);
    }
}
