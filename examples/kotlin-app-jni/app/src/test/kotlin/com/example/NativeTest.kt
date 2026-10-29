package com.example

import kotlin.test.Test
import kotlin.test.assertEquals

class NativeTest {
    @Test
    fun greetsFromCpp() {
        loadGreeter()
        assertEquals("Hello, test, from C++", greet("test"))
    }
}
