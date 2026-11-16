import org.apache.kafka.common.utils.Utils;
import java.nio.charset.StandardCharsets;
import java.util.*;

/** Emits golden vectors for the capture proxy's partition routing so Rust can match Java bit-for-bit. */
public class GenVectors {
    public static void main(String[] args) {
        List<String> ids = new ArrayList<>();
        // Netty-style long-form channel ids (the real connectionId shape).
        ids.addAll(List.of(
            "0f3c9a1b00000001", "0f3c9a1b00000002", "0f3c9a1b0000000a", "deadbeefcafe0001",
            "00000000000000000000000000000000", "ffffffffffffffffffffffffffffffff"));
        // Edge cases: empty, single bytes, lengths crossing murmur2's 4-byte block boundary.
        ids.addAll(List.of("", "a", "ab", "abc", "abcd", "abcde", "abcdef", "abcdefg", "abcdefgh"));
        // High bytes / multibyte UTF-8 so the UTF-8 encoding step is pinned too.
        ids.addAll(List.of("ÿ", "éè", "中文", "😀", "café"));
        // Deterministic pseudo-random ids for bulk comparison.
        Random r = new Random(20261115L);
        for (int i = 0; i < 48; i++) {
            StringBuilder sb = new StringBuilder();
            int len = 1 + r.nextInt(40);
            for (int j = 0; j < len; j++) sb.append((char) ('!' + r.nextInt(94)));
            ids.add(sb.toString());
        }

        int[][] partitionSets = {
            {0}, {0,1}, {0,1,2}, {3,7,11}, {0,1,2,3,4,5,6,7},
            {2,5}, {0,4,8,12,16,20,24,28,32,36}, {15,14,13,12}
        };

        System.out.println("{");
        System.out.println("  \"generator\": \"org.apache.kafka.common.utils.Utils.toPositive(Utils.murmur2(utf8(connectionId)))\",");
        System.out.println("  \"kafkaClients\": \"4.2.0\",");
        System.out.println("  \"vectors\": [");
        boolean firstV = true;
        for (String id : ids) {
            byte[] b = id.getBytes(StandardCharsets.UTF_8);
            int m = Utils.murmur2(b);
            int pos = Utils.toPositive(m);
            if (!firstV) System.out.println(",");
            firstV = false;
            StringBuilder routes = new StringBuilder();
            boolean firstR = true;
            for (int[] set : partitionSets) {
                // Mirror CaptureRoutingState.validateRoutingPartitions: unique, ascending.
                TreeSet<Integer> sorted = new TreeSet<>();
                for (int p : set) sorted.add(p);
                List<Integer> parts = List.copyOf(sorted);
                int chosen = parts.get(pos % parts.size());
                if (!firstR) routes.append(", ");
                firstR = false;
                routes.append("{\"partitions\": ").append(parts).append(", \"selected\": ").append(chosen).append("}");
            }
            System.out.print("    {\"connectionId\": " + quote(id)
                + ", \"utf8Hex\": \"" + hex(b) + "\""
                + ", \"murmur2\": " + m
                + ", \"toPositive\": " + pos
                + ", \"routes\": [" + routes + "]}");
        }
        System.out.println();
        System.out.println("  ]");
        System.out.println("}");
    }

    static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) sb.append(String.format("%02x", x));
        return sb.toString();
    }

    static String quote(String s) {
        StringBuilder sb = new StringBuilder("\"");
        for (char c : s.toCharArray()) {
            if (c == '"' || c == '\\') sb.append('\\').append(c);
            else if (c < 0x20 || c > 0x7e) sb.append(String.format("\\u%04x", (int) c));
            else sb.append(c);
        }
        return sb.append('"').toString();
    }
}
