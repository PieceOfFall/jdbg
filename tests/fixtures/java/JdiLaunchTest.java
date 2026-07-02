public class JdiLaunchTest {
    public static void main(String[] args) {
        int count = 3;
        String label = "hello-jdi-launch";
        int sum = 0;
        for (int i = 0; i < count; i++) {
            sum += i;
        }
        System.out.println(label + " sum=" + sum);
    }
}
