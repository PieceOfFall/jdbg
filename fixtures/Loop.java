public class Loop {
    public static void main(String[] args) throws InterruptedException {
        int n = 0;
        while (true) {
            n++;
            Thread.sleep(500);  // long-running so we can attach
        }
    }
}
