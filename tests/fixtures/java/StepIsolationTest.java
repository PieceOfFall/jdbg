public class StepIsolationTest {
    private static volatile boolean startNoise;
    private static volatile boolean keepRunning = true;

    public static void main(String[] args) throws Exception {
        Thread noise = new Thread(new Runnable() {
            @Override
            public void run() {
                while (keepRunning) {
                    if (startNoise) {
                        noise();
                    } else {
                        Thread.yield();
                    }
                }
            }
        }, "step-noise");
        noise.setDaemon(true);
        noise.start();

        Thread target = new Thread(new Runnable() {
            @Override
            public void run() {
                target();
            }
        }, "step-target");
        target.start();
        target.join();
        keepRunning = false;
    }

    private static void target() {
        int marker = 1; // STEP_ISOLATION_TARGET_BREAK
        startNoise = true;
        marker++; // STEP_ISOLATION_TARGET_STEP
        System.out.println(marker);
    }

    private static void noise() {
        int value = 1; // STEP_ISOLATION_NOISE_ENTRY
        if (value == 0) {
            System.out.println(value);
        }
    }
}
