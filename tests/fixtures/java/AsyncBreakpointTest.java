public class AsyncBreakpointTest {
    public static void main(String[] args) throws Exception {
        AsyncBreakpointTest target = new AsyncBreakpointTest();
        Thread worker = new Thread(new Runnable() {
            @Override
            public void run() {
                try {
                    Thread.sleep(1500);
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
