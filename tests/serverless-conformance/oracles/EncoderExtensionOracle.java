import java.util.*;
import java.nio.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.security.*;
import com.fasterxml.jackson.databind.ObjectMapper;
import org.owasp.encoder.Encode;
public class EncoderExtensionOracle {
 static String encode(String name,String input){return switch(name){case "json"->Encode.forJson(input);case "xml-1.1"->Encode.forXml11(input);case "xml-1.1-content"->Encode.forXml11Content(input);case "xml-1.1-attribute"->Encode.forXml11Attribute(input);default->throw new IllegalArgumentException();};}
 static List<Integer> units(String s){List<Integer> a=new ArrayList<>();for(char c:s.toCharArray())a.add((int)c);return a;}
 public static void main(String[] args)throws Exception{
  Map<String,Object> result=new TreeMap<>();
  for(String name:List.of("json","xml-1.1","xml-1.1-content","xml-1.1-attribute")){
   List<String> ascii=new ArrayList<>();Map<Integer,String> overrides=new TreeMap<>();MessageDigest hash=MessageDigest.getInstance("SHA-256");
   for(int c=0;c<=0x10ffff;c++){
    if(c>=0xd800&&c<=0xdfff)continue;
    String input=new String(Character.toChars(c)),out=encode(name,input);byte[] b=out.getBytes(StandardCharsets.UTF_8);
    hash.update(ByteBuffer.allocate(4).putInt(b.length).array());hash.update(b);
    if(c<256)ascii.add(out);else if(!out.equals(input))overrides.put(c,out);
   }
   Map<String,String> sequences=new TreeMap<>();
   for(String input:List.of("</script>\"'&/\\", "x\ny\r\t", "\u0001a\u00017\u00010", "\u0085\u009f\u2028\u2029", "\ud83d\ude00", "\u007f\u0080\u009f", "\u0000", "\ufffe\uffff"))sequences.put(input,encode(name,input));
   List<Object> invalid=new ArrayList<>();
   for(String input:List.of("\ud800","\udfff","a\ud800b","\udfff\ud800","\ud800\ud800"))invalid.add(Map.of("input",units(input),"output",units(encode(name,input))));
   result.put(name,Map.of("ascii",ascii,"overrides",overrides,"allUnicodeSha256",HexFormat.of().formatHex(hash.digest()),"sequences",sequences,"invalidUtf16",invalid));
  }
  new ObjectMapper().writerWithDefaultPrettyPrinter().writeValue(Path.of(args[0]).toFile(),result);
 }
}
