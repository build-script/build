package com.example.compose

import android.os.Bundle
import android.os.Parcelable
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import kotlinx.parcelize.Parcelize
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json

@Parcelize
@Serializable
data class Build(val tool: String, val plugins: List<String>) : Parcelable

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        val build = Build("xkit", listOf("compose", "parcelize", "serialization"))
        // a round trip through a Bundle proves the generated Parcelable code
        val parceled = Bundle().apply { putParcelable("build", build) }.getParcelable<Build>("build")
        val json = Json.encodeToString(parceled)

        setContent {
            MaterialTheme {
                Surface(modifier = Modifier.fillMaxSize()) {
                    Greeting(json)
                }
            }
        }
    }
}

@Composable
fun Greeting(json: String) {
    var clicks by rememberSaveable { mutableIntStateOf(0) }
    Column(
        modifier = Modifier.padding(24.dp),
        verticalArrangement = Arrangement.spacedBy(16.dp),
        horizontalAlignment = Alignment.Start,
    ) {
        Text(BuildConfig.GREETING, style = MaterialTheme.typography.headlineSmall)
        Text(json)
        Text("answer = ${BuildConfig.ANSWER}")
        Button(onClick = { clicks++ }) { Text("Clicked $clicks times") }
    }
}
