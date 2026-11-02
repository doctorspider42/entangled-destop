/* OpenGL through Zink on our Vulkan stack (ADR-0009 phase D): a WGL context
 * in a window, the renderer string, 300 frames of a spinning triangle and a
 * read-back of one pixel the triangle covers. Run beside Mesa's opengl32.dll
 * and libgallium_wgl.dll with GALLIUM_DRIVER=zink.
 * Prints "GL-TEST PASS ..." or "GL-TEST FAIL ...". */
#include <windows.h>
#include <GL/gl.h>
#include <math.h>
#include <stdio.h>

static LRESULT CALLBACK proc(HWND w, UINT m, WPARAM a, LPARAM b)
{
    return DefWindowProcA(w, m, a, b);
}

int main(void)
{
    WNDCLASSA wc = {0};
    wc.style = CS_OWNDC;
    wc.lpfnWndProc = proc;
    wc.hInstance = GetModuleHandleA(NULL);
    wc.lpszClassName = "gltest";
    RegisterClassA(&wc);
    HWND win = CreateWindowA("gltest", "gl-test", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 100, 100,
                             640, 480, NULL, NULL, wc.hInstance, NULL);
    HDC dc = GetDC(win);
    PIXELFORMATDESCRIPTOR pfd = {0};
    pfd.nSize = sizeof pfd;
    pfd.nVersion = 1;
    pfd.dwFlags = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
    pfd.iPixelType = PFD_TYPE_RGBA;
    pfd.cColorBits = 32;
    pfd.cDepthBits = 24;
    int format = ChoosePixelFormat(dc, &pfd);
    if (!format || !SetPixelFormat(dc, format, &pfd)) {
        printf("GL-TEST FAIL pixel format %lu\n", GetLastError());
        return 1;
    }
    HGLRC rc = wglCreateContext(dc);
    if (!rc || !wglMakeCurrent(dc, rc)) {
        printf("GL-TEST FAIL context %lu\n", GetLastError());
        return 1;
    }
    printf("vendor   %s\nrenderer %s\nversion  %s\n", (const char *)glGetString(GL_VENDOR),
           (const char *)glGetString(GL_RENDERER), (const char *)glGetString(GL_VERSION));
    RECT r;
    GetClientRect(win, &r);
    glViewport(0, 0, r.right, r.bottom);
    LARGE_INTEGER f, t0, t1;
    QueryPerformanceFrequency(&f);
    QueryPerformanceCounter(&t0);
    const int frames = 300;
    unsigned char px[4] = {0};
    for (int i = 0; i < frames; i++) {
        MSG msg;
        while (PeekMessageA(&msg, NULL, 0, 0, PM_REMOVE))
            DispatchMessageA(&msg);
        glClearColor(0.1f, 0.1f, 0.3f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT);
        glLoadIdentity();
        glRotatef(i * 2.0f, 0, 0, 1);
        glBegin(GL_TRIANGLES);
        glColor3f(1, 0, 0);
        glVertex2f(0.0f, 0.8f);
        glColor3f(0, 1, 0);
        glVertex2f(-0.7f, -0.6f);
        glColor3f(0, 0, 1);
        glVertex2f(0.7f, -0.6f);
        glEnd();
        if (i == frames - 1)
            glReadPixels(r.right / 2, r.bottom / 2, 1, 1, GL_RGBA, GL_UNSIGNED_BYTE, px);
        SwapBuffers(dc);
    }
    QueryPerformanceCounter(&t1);
    double s = (double)(t1.QuadPart - t0.QuadPart) / f.QuadPart;
    GLenum err = glGetError();
    /* The centre is inside the triangle at every angle: not the clear colour. */
    int drawn = !(px[0] == 26 && px[1] == 26 && px[2] == 77) && (px[0] | px[1] | px[2]);
    if (err != GL_NO_ERROR || !drawn) {
        printf("GL-TEST FAIL error 0x%x centre %u,%u,%u\n", err, px[0], px[1], px[2]);
        return 1;
    }
    printf("GL-TEST PASS %d frames in %.2f s (%.1f fps), centre %u,%u,%u\n", frames, s,
           frames / s, px[0], px[1], px[2]);
    wglMakeCurrent(NULL, NULL);
    wglDeleteContext(rc);
    return 0;
}
