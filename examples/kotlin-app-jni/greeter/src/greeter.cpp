#include <jni.h>

#include <string>

// greet(name) of com.example.Native, in C++
extern "C" JNIEXPORT jstring JNICALL Java_com_example_NativeKt_greet(JNIEnv *env, jclass, jstring name) {
    const char *chars = env->GetStringUTFChars(name, nullptr);
    std::string greeting = "Hello, " + std::string(chars) + ", from C++";
    env->ReleaseStringUTFChars(name, chars);
    return env->NewStringUTF(greeting.c_str());
}
