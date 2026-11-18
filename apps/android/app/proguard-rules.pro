-keep public class org.waypane.app.NativeBridge { *; }
-keepclassmembers class org.waypane.app.NativeBridge {
    native <methods>;
}
-keepattributes Signature, InnerClasses, EnclosingMethod, RuntimeVisibleAnnotations, RuntimeVisibleTypeAnnotations
-keepclassmembers,allowobfuscation @kotlinx.serialization.Serializable class ** { *; }
-if @kotlinx.serialization.Serializable class **
-keepclassmembers class <1> {
    static <1>$Companion Companion;
}
