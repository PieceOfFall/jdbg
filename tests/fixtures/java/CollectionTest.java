import java.util.ArrayList;
import java.util.List;

public class CollectionTest {
    public static void main(String[] args) {
        List<String> fruits = new ArrayList<>();
        fruits.add("apple");
        fruits.add("banana");
        fruits.add("cherry");
        int size = fruits.size();
        String first = fruits.get(0);
        System.out.println("done: " + size + " " + first);
    }
}
