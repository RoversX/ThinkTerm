# R8's rules for the release build, on top of the libraries' own.

# UniFFI's generated bindings reach the Rust library through JNA, which finds
# structure fields, callback methods and native entry points by reflection:
# keep JNA and the bindings whole.
-keep class com.sun.jna.** { *; }
-keepclassmembers class * extends com.sun.jna.** { public *; }
-keep class com.roversx.thinkterm.core.** { *; }

# JNA's desktop helpers name AWT, which Android does not have.
-dontwarn java.awt.**

# Tink, under security-crypto, is annotated with these, which are needed
# only to compile it and are not shipped with it.
-dontwarn com.google.errorprone.annotations.**
-dontwarn javax.annotation.**
