/**
 * Fixture for thread-suspend-policy tests (jdb native `stop thread`).
 *
 * - "worker" hits the breakpoint in doWork() and gets suspended.
 * - "heartbeat" keeps incrementing heartbeatCount forever. It writes NOTHING
 *   to stdout, so the debuggee never interleaves output with jdb's event
 *   banner while a thread breakpoint is being reported.
 * - main() blocks forever (sleep) so the VM stays alive after "worker" is
 *   suspended. Under a thread-suspend policy main/heartbeat must keep running;
 *   the program must NOT self-terminate (that would race VM exit against the
 *   debugger and corrupt the captured transcript).
 */
public class ThreadTest {
    static volatile boolean alive = true;
    static volatile int heartbeatCount = 0;

    public static void main(String[] args) throws Exception {
        Thread heartbeat = new Thread(() -> {
            while (alive) {
                heartbeatCount++;
                try { Thread.sleep(20); } catch (InterruptedException e) { break; }
            }
        }, "heartbeat");
        heartbeat.setDaemon(true);
        heartbeat.start();

        Thread worker = new Thread(() -> doWork(), "worker");
        worker.setDaemon(true);
        worker.start();

        // Keep the VM alive without depending on worker/heartbeat finishing:
        // under thread policy main is NOT suspended, so it runs to here and
        // parks in sleep (the test kills the jdb session when done).
        Thread.sleep(600000);
    }

    static void doWork() {
        int x = 1;
        int y = x + 1;
        int z = y + 1;
        // Block forever so "worker" stays parked and visible in `threads`
        // even if the breakpoint fails to arm (rules out the race where worker
        // runs doWork to completion and the thread disappears). No stdout, so
        // it cannot interleave with jdb's event banner.
        while (alive) {
            try { Thread.sleep(100); } catch (InterruptedException e) { break; }
        }
    }
}
