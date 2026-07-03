public class AsyncBreakpointTest {
    public static void main(String[] args) throws Exception {
        // The worker blocks on a gate file instead of sleeping a fixed duration, so
        // the test controls exactly when the breakpoint becomes reachable. While the
        // gate is absent the breakpoint cannot be hit (the first `run` is a
        // deterministic timeout); once the test creates the gate the worker runs into
        // the breakpoint asynchronously. No wall-clock race, no sleep tuning.
        final java.io.File gate = new java.io.File(args[0]);
        AsyncBreakpointTest target = new AsyncBreakpointTest();
        Thread worker = new Thread(new Runnable() {
            @Override
            public void run() {
                try {
                    while (!gate.exists()) {
                        Thread.sleep(20);
                    }
                    target.hit();
                    Thread.sleep(300000);
                } catch (InterruptedException ignored) {
                }
            }
        }, "delayed-worker");
        worker.start();
        Thread.sleep(300000);
    }

    private void hit() {
        int marker = 42; // ASYNC_BREAKPOINT
        System.out.println("marker=" + marker);
    }
}
