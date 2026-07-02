import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

public class StructuredInspectTest {
    enum Mode {
        ACTIVE,
        IDLE
    }

    static final class Node {
        String name;
        Node self;
        List<String> tags;
        Map<String, Integer> counts;
        Mode mode;
        int[] numbers;
        String ready;
    }

    public static void main(String[] args) throws Exception {
        Node root = new Node();
        root.name = "root";
        root.self = root;
        root.tags = new ArrayList<>();
        root.tags.add("alpha");
        root.tags.add("beta");
        root.tags.add("gamma");
        root.counts = new LinkedHashMap<>();
        root.counts.put("one", 1);
        root.counts.put("two", 2);
        root.counts.put("three", 3);
        root.mode = Mode.ACTIVE;
        root.numbers = new int[] {1, 2, 3, 4};
        root.ready = "ready";
        System.out.println(root.name);
        Thread.sleep(300000);
    }
}
