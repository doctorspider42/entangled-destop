/*
 * gltest.exe — the smallest program that shows whether OpenGL works in a
 * Windows XP guest (ADR-0013): a window, a pixel format, a context, and a
 * lit, spinning, colour-per-vertex cube drawn the 1997 way, sixty times a
 * second. Its title bar says whose OpenGL drew it (GL_RENDERER) and how
 * fast, so a screenshot of the guest answers both questions.
 *
 * Freestanding C, no C runtime; built with clang, see ../build.sh.
 */
#include <windows.h>
#include <GL/gl.h>

int _fltused = 1;

static LRESULT CALLBACK window_proc(HWND window, UINT message, WPARAM wparam, LPARAM lparam)
{
    if (message == WM_DESTROY) {
        PostQuitMessage(0);
        return 0;
    }
    return DefWindowProcA(window, message, wparam, lparam);
}

static void cube(void)
{
    static const GLfloat corners[8][3] = {
        {-1, -1, -1}, {1, -1, -1}, {1, 1, -1}, {-1, 1, -1},
        {-1, -1, 1},  {1, -1, 1},  {1, 1, 1},  {-1, 1, 1},
    };
    static const unsigned char faces[6][4] = {
        {0, 3, 2, 1}, {4, 5, 6, 7}, {0, 1, 5, 4}, {2, 3, 7, 6}, {1, 2, 6, 5}, {0, 4, 7, 3},
    };
    static const GLfloat normals[6][3] = {
        {0, 0, -1}, {0, 0, 1}, {0, -1, 0}, {0, 1, 0}, {1, 0, 0}, {-1, 0, 0},
    };
    int face, corner;
    glBegin(GL_QUADS);
    for (face = 0; face < 6; face++) {
        glNormal3fv(normals[face]);
        for (corner = 0; corner < 4; corner++) {
            const GLfloat *v = corners[faces[face][corner]];
            glColor3f(v[0] * 0.5f + 0.5f, v[1] * 0.5f + 0.5f, v[2] * 0.5f + 0.5f);
            glVertex3fv(v);
        }
    }
    glEnd();
}

static void title(HWND window, unsigned fps)
{
    /* "123 fps - <renderer>": the number first, where a narrow title bar
     * still shows it. */
    char text[200];
    const char *renderer = (const char *)glGetString(GL_RENDERER);
    const char *middle = " fps - ";
    char *out = text;
    if (fps >= 1000) {
        *out++ = (char)('0' + fps / 1000 % 10);
    }
    if (fps >= 100) {
        *out++ = (char)('0' + fps / 100 % 10);
    }
    if (fps >= 10) {
        *out++ = (char)('0' + fps / 10 % 10);
    }
    *out++ = (char)('0' + fps % 10);
    while (*middle) {
        *out++ = *middle++;
    }
    while (renderer != NULL && *renderer && out < text + 190) {
        *out++ = *renderer++;
    }
    *out = 0;
    SetWindowTextA(window, text);
}

void entry(void)
{
    static const GLfloat light[4] = {0.4f, 0.6f, 1.0f, 0.0f};
    WNDCLASSA class_;
    PIXELFORMATDESCRIPTOR format;
    HWND window;
    HDC dc;
    HGLRC context;
    MSG message;
    DWORD started, frames = 0, second;
    unsigned i;
    int chosen;

    for (i = 0; i < sizeof(class_); i++) {
        ((char *)&class_)[i] = 0;
    }
    class_.style = CS_OWNDC;
    class_.lpfnWndProc = window_proc;
    class_.hInstance = GetModuleHandleA(NULL);
    class_.hCursor = LoadCursorA(NULL, (LPCSTR)IDC_ARROW);
    class_.lpszClassName = "EntangledGlTest";
    RegisterClassA(&class_);
    window = CreateWindowExA(0, "EntangledGlTest", "OpenGL", WS_OVERLAPPEDWINDOW | WS_VISIBLE, 80,
                             60, 480, 400, NULL, NULL, class_.hInstance, NULL);
    dc = GetDC(window);

    for (i = 0; i < sizeof(format); i++) {
        ((char *)&format)[i] = 0;
    }
    format.nSize = sizeof(format);
    format.nVersion = 1;
    format.dwFlags = PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER;
    format.iPixelType = PFD_TYPE_RGBA;
    format.cColorBits = 32;
    format.cDepthBits = 24;
    chosen = ChoosePixelFormat(dc, &format);
    if (chosen == 0 || !SetPixelFormat(dc, chosen, &format) ||
        (context = wglCreateContext(dc)) == NULL || !wglMakeCurrent(dc, context)) {
        MessageBoxA(window, "No OpenGL context.", "gltest", MB_OK);
        ExitProcess(1);
    }

    glEnable(GL_DEPTH_TEST);
    glEnable(GL_LIGHTING);
    glEnable(GL_LIGHT0);
    glEnable(GL_COLOR_MATERIAL);
    glLightfv(GL_LIGHT0, GL_POSITION, light);
    title(window, 0);
    started = second = GetTickCount();

    for (;;) {
        RECT client;
        DWORD now;
        while (PeekMessageA(&message, NULL, 0, 0, PM_REMOVE)) {
            if (message.message == WM_QUIT) {
                wglMakeCurrent(NULL, NULL);
                wglDeleteContext(context);
                ExitProcess(0);
            }
            TranslateMessage(&message);
            DispatchMessageA(&message);
        }
        now = GetTickCount();
        GetClientRect(window, &client);
        glViewport(0, 0, client.right, client.bottom);
        glClearColor(0.08f, 0.09f, 0.14f, 1.0f);
        glClear(GL_COLOR_BUFFER_BIT | GL_DEPTH_BUFFER_BIT);
        glMatrixMode(GL_PROJECTION);
        glLoadIdentity();
        glFrustum(-0.6, 0.6, -0.5, 0.5, 1.0, 20.0);
        glMatrixMode(GL_MODELVIEW);
        glLoadIdentity();
        glTranslatef(0.0f, 0.0f, -5.0f);
        glRotatef((GLfloat)(now - started) * 0.05f, 0.3f, 1.0f, 0.2f);
        cube();
        SwapBuffers(dc);
        frames++;
        if (now - second >= 1000) {
            title(window, frames * 1000 / (now - second));
            frames = 0;
            second = now;
        }
    }
}
