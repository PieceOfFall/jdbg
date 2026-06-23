public class Throw {
    public static void main(String[] args) {
        String s = null;
        System.out.println(s.length());  // triggers uncaught NullPointerException
    }
}
