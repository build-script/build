package com.example

external fun greet(name: String): String

fun loadGreeter() = System.loadLibrary("greeter")
