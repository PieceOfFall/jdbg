public class WatchTwiceTest {
    static String name = "initial";

    public static void main(String[] args) {
        String before = name;
        name = "first";
        name = "second";
        System.out.println(before + " -> " + name);
    }
}
