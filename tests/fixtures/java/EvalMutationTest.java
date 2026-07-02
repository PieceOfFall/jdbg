public class EvalMutationTest {
    static int gate;
    static int afterCompute;

    static final class Box {
        int count;

        int add(int value) {
            return count + value;
        }
    }

    static int staticAdd(int left, int right) {
        return left + right;
    }

    public static void main(String[] args) throws Exception {
        Box box = new Box();
        box.count = 4;
        int[] values = new int[] {1, 2, 3};
        int result = compute(box, values);
        afterCompute = result;
        System.out.println("result=" + result + " count=" + box.count + " value1=" + values[1]);
        Thread.sleep(300000);
    }

    static int compute(Box box, int[] values) {
        int local = 6;
        gate++;
        int before = box.add(values[1]);
        return before + local;
    }
}
