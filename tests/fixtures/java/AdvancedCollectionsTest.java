import java.util.ArrayDeque;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.LinkedHashSet;
import java.util.LinkedList;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;
import java.util.TreeSet;

public class AdvancedCollectionsTest {
    static final class Holder {
        LinkedList<String> linkedList = new LinkedList<>();
        ArrayDeque<String> deque = new ArrayDeque<>();
        HashSet<String> hashSet = new HashSet<>();
        LinkedHashSet<String> linkedSet = new LinkedHashSet<>();
        TreeMap<String, Integer> treeMap = new TreeMap<>();
        TreeSet<String> treeSet = new TreeSet<>();
        List<String> unmodifiableList;
        Map<String, Integer> unmodifiableMap;
    }

    public static void main(String[] args) throws Exception {
        Holder holder = new Holder();
        holder.linkedList.addAll(Arrays.asList("linked-a", "linked-b", "linked-c"));
        holder.deque.addAll(Arrays.asList("deque-a", "deque-b", "deque-c"));
        holder.hashSet.addAll(Arrays.asList("set-a", "set-b"));
        holder.linkedSet.addAll(Arrays.asList("linked-set-a", "linked-set-b"));
        holder.treeMap.put("one", 1);
        holder.treeMap.put("two", 2);
        holder.treeSet.add("tree-a");
        holder.treeSet.add("tree-b");
        holder.unmodifiableList = Collections.unmodifiableList(holder.linkedList);
        holder.unmodifiableMap = Collections.unmodifiableMap(new LinkedHashMap<String, Integer>(holder.treeMap));
        System.out.println(holder.linkedList.size());
        Thread.sleep(300000);
    }
}
