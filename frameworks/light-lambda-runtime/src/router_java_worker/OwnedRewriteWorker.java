import com.fasterxml.jackson.databind.ObjectMapper;
import java.io.*;
import java.util.*;
import java.util.regex.*;

// Application-owned rewrite-only child. No HTTP, destinations or credentials.
// Resource values remain qualification-only until the Router profile is admitted.
public final class OwnedRewriteWorker {
  static final int FRAME = 2 * 1024 * 1024; // experimental, not production admission
  static final int OUTPUT = 16384;         // UTF16 units, checked before append
  static final ObjectMapper JSON = new ObjectMapper();
  record Rule(Pattern pattern, String replacement) {}
  static String frameDigest;
  static Map<?,?> read(DataInputStream in) throws Exception {
    int n = in.readInt();
    if (n < 0 || n > FRAME) throw new IOException("frame");
    byte[] bytes = new byte[n];
    in.readFully(bytes);
    frameDigest = java.util.HexFormat.of().formatHex(java.security.MessageDigest.getInstance("SHA-256").digest(bytes));
    return JSON.readValue(bytes, Map.class);
  }
  static void write(DataOutputStream out, Object value) throws Exception {
    byte[] bytes = JSON.writeValueAsBytes(value);
    if (bytes.length > FRAME) throw new IOException("frame");
    out.writeInt(bytes.length); out.write(bytes); out.flush();
  }
  static void append(StringBuilder out, String value) {
    if (value != null) {
      if (value.length() > OUTPUT - out.length()) throw new IllegalStateException("output");
      out.append(value);
    }
  }
  static void replacement(StringBuilder out, Matcher m, String replacement) {
    for (int i = 0; i < replacement.length();) {
      char c = replacement.charAt(i++);
      if (c == '\\') {
        if (i == replacement.length()) throw new IllegalArgumentException("escape");
        append(out, String.valueOf(replacement.charAt(i++)));
      } else if (c == '$') {
        if (i == replacement.length()) throw new IllegalArgumentException("group");
        c = replacement.charAt(i++);
        String group;
        if (c == '{') {
          int start = i;
          while (i < replacement.length() && ((replacement.charAt(i) >= 'a' && replacement.charAt(i) <= 'z') ||
                 (replacement.charAt(i) >= 'A' && replacement.charAt(i) <= 'Z') ||
                 (replacement.charAt(i) >= '0' && replacement.charAt(i) <= '9'))) i++;
          if (i == start || i == replacement.length() || replacement.charAt(i) != '}' || Character.isDigit(replacement.charAt(start)))
            throw new IllegalArgumentException("group name");
          group = m.group(replacement.substring(start, i++));
        } else {
          if (c < '0' || c > '9') throw new IllegalArgumentException("group");
          int ref = c - '0';
          while (i < replacement.length() && replacement.charAt(i) >= '0' && replacement.charAt(i) <= '9') {
            int next = ref * 10 + replacement.charAt(i) - '0';
            if (next > m.groupCount()) break;
            ref = next; i++;
          }
          group = m.group(ref);
        }
        append(out, group);
      } else append(out, String.valueOf(c));
    }
  }
  static String boundedReplace(Pattern p, String path, String replacement) {
    Matcher m = p.matcher(path);
    StringBuilder out = new StringBuilder();
    int end = 0;
    while (m.find()) {
      append(out, path.substring(end, m.start()));
      replacement(out, m, replacement);
      end = m.end();
    }
    append(out, path.substring(end));
    return out.toString();
  }
  static List<Integer> units(String value) {
    List<Integer> result = new ArrayList<>(value.length());
    for (int i = 0; i < value.length(); i++) result.add((int)value.charAt(i));
    return result;
  }
  static boolean validUtf16(String value) {
    for (int i = 0; i < value.length(); i++) {
      char c = value.charAt(i);
      if (Character.isHighSurrogate(c)) {
        if (++i == value.length() || !Character.isLowSurrogate(value.charAt(i))) return false;
      } else if (Character.isLowSurrogate(c)) return false;
    }
    return true;
  }
  public static void main(String[] args) throws Exception {
    DataInputStream in = new DataInputStream(System.in);
    DataOutputStream out = new DataOutputStream(System.out);
    Map<?,?> init = read(in);
    String captureDigest = frameDigest;
    ByteArrayOutputStream programBytes = new ByteArrayOutputStream();
    DataOutputStream program = new DataOutputStream(programBytes);
    program.write("WP12/rewrite-program/v1\0".getBytes(java.nio.charset.StandardCharsets.US_ASCII));
    program.writeInt(((List<?>)init.get("rules")).size());
    for (Object raw : (List<?>)init.get("rules")) {
      Map<?,?> rule = (Map<?,?>)raw;
      for (String key : List.of("pattern", "replacement")) {
        String value = (String)rule.get(key); program.writeInt(value.length());
        for (int i=0;i<value.length();i++) program.writeChar(value.charAt(i));
      }
    }
    String programDigest = java.util.HexFormat.of().formatHex(java.security.MessageDigest.getInstance("SHA-256").digest(programBytes.toByteArray()));
    long epoch = init.get("epoch") instanceof Number n ? n.longValue() : 0;
    if (Boolean.TRUE.equals(init.get("initHang"))) Thread.sleep(60000);
    List<Rule> rules = new ArrayList<>();
    long started = System.nanoTime();
    try {
      for (Object raw : (List<?>)init.get("rules")) {
        Map<?,?> rule = (Map<?,?>)raw;
        rules.add(new Rule(Pattern.compile((String)rule.get("pattern")), (String)rule.get("replacement")));
      }
      write(out, Map.of("ready", true, "rules", rules.size(), "compileNanos", System.nanoTime()-started, "environmentEntries", System.getenv().size(), "captureSha256", captureDigest, "epoch", epoch, "programSha256", programDigest));
    } catch (Throwable error) {
      write(out, Map.of("ready", false, "error", error.getClass().getSimpleName()));
      return;
    }
    while (true) {
      Map<?,?> request;
      try { request = read(in); } catch (EOFException end) { return; }
      if (!(request.get("epoch") instanceof Number e) || e.longValue() != epoch) throw new IOException("epoch");
      Object id = request.get("id");
      if (!(id instanceof Integer || id instanceof Long) || ((Number)id).longValue() < 1)
        throw new IOException("request id");
      String fault = (String)request.get("fault");
      if (!Boolean.getBoolean("wp12.qualification")) fault = null;
      if ("hang".equals(fault)) { Thread.sleep(60000); continue; }
      if ("death".equals(fault)) System.exit(7);
      if ("oversized".equals(fault)) { out.writeInt(FRAME+1); out.flush(); continue; }
      if ("malformed".equals(fault)) { out.writeInt(1);out.writeByte('{');out.flush();continue; }
      if ("stale".equals(fault)) { write(out, Map.of("id", -1, "epoch", epoch, "units", List.of())); continue; }
      try {
        if ("panic".equals(fault)) throw new AssertionError("qualification panic");
        String path = (String)request.get("path");
        if (path.length() > 65536) throw new IllegalStateException("input");
        String result = path;
        for (Rule rule : rules) if (rule.pattern.matcher(path).matches()) {
          result = boundedReplace(rule.pattern, path, rule.replacement);
          // Bounded probe only: actual Java replaceAll is the independent oracle.
          if (Boolean.TRUE.equals(request.get("compare")) &&
              !result.equals(rule.pattern.matcher(path).replaceAll(rule.replacement)))
            throw new AssertionError("replacement mismatch");
          break;
        }
        if (result.length() > OUTPUT) throw new IllegalStateException("output");
        write(out, Map.of("id", id, "epoch", epoch, "units", units(result), "validUtf16", validUtf16(result)));
      } catch (Throwable error) {
        write(out, Map.of("id", id, "epoch", epoch, "error", error.getClass().getSimpleName()));
      }
    }
  }
}
