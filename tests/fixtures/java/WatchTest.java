/**
 * Fixture for field watchpoint tests.
 * Has a mutable field that gets modified in main().
 */
public class WatchTest {
    static String name = "initial";

    public static void main(String[] args) {
        String before = name;
        name = "modified";
        String after = name;
        System.out.println("done: " + before + " -> " + after);
    }
}
