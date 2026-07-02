public class MethodEventTest {
    public static void main(String[] args) {
        int result = work(2, "go");
        System.out.println("result=" + result);
    }

    static int work(int count, String label) {
        int sum = count + label.length();
        return sum;
    }
}
