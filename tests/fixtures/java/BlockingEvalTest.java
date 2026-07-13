public class BlockingEvalTest {
    private final java.io.File gate;

    private BlockingEvalTest(String gatePath) {
        this.gate = new java.io.File(gatePath);
    }

    public static void main(String[] args) throws Exception {
        BlockingEvalTest target = new BlockingEvalTest(args[0]);
        target.stopHere();
        Thread.sleep(300000);
    }

    private void stopHere() {
        int marker = 42; // BLOCKING_EVAL_STOP
        System.out.println("marker=" + marker);
    }

    int waitForGate() throws InterruptedException {
        while (!gate.exists()) {
            Thread.sleep(20);
        }
        return 7;
    }
}
