#include <raylib.h>

#include <cstdlib>

// with a number of frames as argument it closes by itself
int main(int argc, char** argv) {
    int frames = argc > 1 ? std::atoi(argv[1]) : -1;

    InitWindow(640, 360, "build-script");
    SetTargetFPS(60);

    for (int frame = 0; !WindowShouldClose() && frame != frames; frame++) {
        BeginDrawing();
        ClearBackground(RAYWHITE);
        DrawText("Built with build-script", 160, 160, 28, DARKGRAY);
        EndDrawing();
    }

    CloseWindow();
    return 0;
}
