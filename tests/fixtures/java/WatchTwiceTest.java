public class WatchTwiceTest {
    static String phase;
    static String name = "initial";

    public static void main(String[] args) {
        String before = name;
        phase = "ready";
        name = "first";
        name = "second";
        System.out.println(before + " -> " + name);
    }
}
