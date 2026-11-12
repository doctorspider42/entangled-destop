/*
 * entgl.dll — the OpenGL driver of the Entangled display driver for
 * Windows XP (ADR-0013): an "installable client driver", the DLL
 * opengl32.dll loads into every program that uses OpenGL on this display.
 *
 * It draws nothing. Every OpenGL call is written into a buffer as one
 * record — an opcode and its arguments — and the buffer goes to the host,
 * which makes the same calls on its own OpenGL
 * (crates/gl-passthrough). The buffer leaves when it is full, when the
 * program asks a question (the answer comes back with it), and when it
 * swaps buffers; the picture then appears in the window because the host
 * puts it in the framebuffer where the window is.
 *
 * Two kinds of functions:
 *
 *   - those whose arguments are all numbers, and the fixed-size vector
 *     forms: generated (gl_stubs.inc, by gen.py);
 *   - everything that takes a pointer, answers, or is the client's own
 *     state: below, by hand. What OpenGL calls client state — pixel-store
 *     modes and the vertex-array pointers — never leaves this DLL: images
 *     are sent tightly packed, and a draw from arrays sends exactly the
 *     vertices it reads.
 *
 * Not here yet: evaluators (glMap*), feedback and selection, glGetTexImage,
 * pixel maps, and every extension — it is OpenGL 1.1 and says so.
 *
 * Freestanding C, no C runtime; built with clang, see ../build.sh.
 */
#include <stdarg.h>
#include <stddef.h>

#include <windows.h>
#include <GL/gl.h>

#include "entgl.h"

/* What opengl32.dll knows a context by: ours to choose. */
typedef ULONG_PTR DHGLRC;

int _fltused = 1;

void *__cdecl memset(void *destination, int value, size_t count)
{
    unsigned char *out = (unsigned char *)destination;
    while (count--) {
        *out++ = (unsigned char)value;
    }
    return destination;
}

void *__cdecl memcpy(void *destination, const void *source, size_t count)
{
    unsigned char *out = (unsigned char *)destination;
    const unsigned char *in = (const unsigned char *)source;
    while (count--) {
        *out++ = *in++;
    }
    return destination;
}

/* ---- the client's own state ------------------------------------------ */

typedef struct {
    int enabled;
    int size;
    GLenum type;
    int stride;
    const unsigned char *pointer;
} Array;

typedef struct {
    int swap_bytes, lsb_first, row_length, skip_rows, skip_pixels, alignment;
} PixelStore;

typedef struct {
    Array vertex, normal, color, index, texcoord, edge;
    PixelStore pack, unpack;
} Client;

#define BUFFER_WORDS 16384u          /* 64 KiB a context */
#define MAX_RECORD_WORDS 0xb0000u    /* what fits the device's command area */
#define ONE_RECORD_BYTES (1u << 20)  /* images larger than this go in strips */
#define CLIENT_STACK 16

typedef struct Context {
    unsigned id;          /* the host's name for it */
    HDC dc;
    HWND window;
    int double_buffered;
    unsigned *buffer;     /* own, or a one-record heap block */
    unsigned capacity;    /* in words */
    unsigned used;        /* in words; the first four are the SELECT record */
    int big;              /* buffer is a heap block, to free after sending */
    Client client;
    Client stack[CLIENT_STACK];
    GLbitfield stack_mask[CLIENT_STACK];
    int stack_depth;
    struct Context *next; /* every context of this process */
    unsigned own[BUFFER_WORDS];
} Context;

static DWORD tls = TLS_OUT_OF_INDEXES;
static HANDLE heap;
static HDC escape_dc;
static CRITICAL_SECTION lock;
static Context *contexts;
/* Where a call with no context writes its arguments. */
static unsigned sink[64];

static Context *current(void)
{
    return (Context *)TlsGetValue(tls);
}

/* ---- the way to the host ---------------------------------------------- */

#ifdef ENTGL_TEST
/*
 * The test build (crates/gl-passthrough/tests/icd.rs): the same driver,
 * compiled for the host, with the escape replaced by a function the test
 * supplies — so the driver and the host's executor meet without a guest
 * between them.
 */
static int (*test_transact)(const unsigned *, unsigned, void *, unsigned);

__declspec(dllexport) void entgl_test_hook(int (*hook)(const unsigned *, unsigned, void *, unsigned))
{
    test_transact = hook;
}
#endif

/* Sends `words` words; returns the bytes of response, or -1. */
static int transact(const unsigned *stream, unsigned words, void *response, unsigned bytes)
{
    int result;
#ifdef ENTGL_TEST
    return test_transact != NULL ? test_transact(stream, words, response, bytes) : -1;
#endif
    if (escape_dc == NULL) {
        HDC dc = CreateDCA("DISPLAY", NULL, NULL, NULL);
        if (InterlockedCompareExchangePointer((PVOID *)&escape_dc, dc, NULL) != NULL) {
            DeleteDC(dc);
        }
    }
    result = ExtEscape(escape_dc, ENTGL_ESCAPE, (int)(words * 4), (LPCSTR)stream, (int)bytes,
                       (LPSTR)response);
    return result > 0 ? result - 1 : -1;
}

/* Sends what the context has buffered; the response, if any is wanted. */
static int flush(Context *c, void *response, unsigned bytes)
{
    int got = 0;
    if (c->used > 4) {
        RECT client;
        unsigned width = 1, height = 1;
        if (c->window != NULL && GetClientRect(c->window, &client)) {
            width = (unsigned)client.right;
            height = (unsigned)client.bottom;
        }
        c->buffer[0] = ENTGL_SELECT | 3u << 16;
        c->buffer[1] = c->id;
        c->buffer[2] = width;
        c->buffer[3] = height;
        got = transact(c->buffer, c->used, response, bytes);
    }
    if (c->big) {
        HeapFree(heap, 0, c->buffer);
        c->buffer = c->own;
        c->capacity = BUFFER_WORDS;
        c->big = 0;
    }
    c->used = 4;
    return got;
}

static unsigned *start_record(Context *c, unsigned opcode, unsigned words)
{
    unsigned *at = c->buffer + c->used;
    if (words >= 0xffff) {
        at[0] = opcode | 0xffffu << 16;
        at[1] = words;
        at += 2;
        c->used += words + 2;
    } else {
        at[0] = opcode | words << 16;
        at += 1;
        c->used += words + 1;
    }
    return at;
}

/* Room for a record of up to 60 argument words: what every generated stub
 * and most of the functions below need. Never fails. */
static unsigned *entgl_command(unsigned opcode, unsigned words)
{
    Context *c = current();
    if (c == NULL || words > 60) {
        return sink;
    }
    if (c->big || c->used + words + 2 > c->capacity) {
        flush(c, NULL, 0);
    }
    return start_record(c, opcode, words);
}

/* Room for a record of any size the device can carry, or NULL. */
static unsigned *entgl_large(Context *c, unsigned opcode, unsigned words)
{
    if (words > MAX_RECORD_WORDS) {
        return NULL;
    }
    if (c->big || c->used + words + 2 > c->capacity) {
        flush(c, NULL, 0);
    }
    if (words + 6 > c->capacity) {
        unsigned *block = (unsigned *)HeapAlloc(heap, 0, (words + 6) * 4);
        if (block == NULL) {
            return NULL;
        }
        c->buffer = block;
        c->capacity = words + 6;
        c->big = 1;
        c->used = 4;
    }
    return start_record(c, opcode, words);
}

static void entgl_put_float(unsigned *at, GLfloat value)
{
    union {
        GLfloat f;
        unsigned u;
    } bits;
    bits.f = value;
    *at = bits.u;
}

static void entgl_put_double(unsigned *at, GLdouble value)
{
    union {
        GLdouble d;
        unsigned u[2];
    } bits;
    bits.d = value;
    at[0] = bits.u[0];
    at[1] = bits.u[1];
}

static GLfloat get_float(unsigned word)
{
    union {
        GLfloat f;
        unsigned u;
    } bits;
    bits.u = word;
    return bits.f;
}

static GLdouble get_double(const unsigned *words)
{
    union {
        GLdouble d;
        unsigned u[2];
    } bits;
    bits.u[0] = words[0];
    bits.u[1] = words[1];
    return bits.d;
}

#include "gl_stubs.inc"

/* A record that is answered: sends everything, returns the words that
 * came back (the rest of `response` is zero). */
static unsigned ask(unsigned opcode, const unsigned *args, unsigned count, unsigned *response,
                    unsigned words)
{
    Context *c = current();
    unsigned *a;
    unsigned i;
    int got;
    for (i = 0; i < words; i++) {
        response[i] = 0;
    }
    if (c == NULL) {
        return 0;
    }
    a = entgl_command(opcode, count);
    for (i = 0; i < count; i++) {
        a[i] = args[i];
    }
    got = flush(c, response, words * 4);
    return got > 0 ? (unsigned)got / 4 : 0;
}

/* ---- sizes ------------------------------------------------------------- */

static unsigned type_bytes(GLenum type)
{
    switch (type) {
    case GL_BYTE:
    case GL_UNSIGNED_BYTE:
        return 1;
    case GL_SHORT:
    case GL_UNSIGNED_SHORT:
        return 2;
    case GL_INT:
    case GL_UNSIGNED_INT:
    case GL_FLOAT:
        return 4;
    case GL_DOUBLE:
        return 8;
    default:
        return 0;
    }
}

static unsigned components(GLenum format)
{
    switch (format) {
    case GL_COLOR_INDEX:
    case GL_STENCIL_INDEX:
    case GL_DEPTH_COMPONENT:
    case GL_RED:
    case GL_GREEN:
    case GL_BLUE:
    case GL_ALPHA:
    case GL_LUMINANCE:
        return 1;
    case GL_LUMINANCE_ALPHA:
        return 2;
    case GL_RGB:
    case GL_BGR_EXT:
        return 3;
    case GL_RGBA:
    case GL_BGRA_EXT:
        return 4;
    default:
        return 0;
    }
}

/* How many values a parameter name carries, across glFog, glLight,
 * glLightModel, glMaterial, glTexParameter, glTexEnv and glTexGen: their
 * names do not collide. */
static unsigned parameter_count(GLenum name)
{
    switch (name) {
    case GL_FOG_COLOR:
    case GL_AMBIENT:
    case GL_DIFFUSE:
    case GL_SPECULAR:
    case GL_POSITION:
    case GL_LIGHT_MODEL_AMBIENT:
    case GL_EMISSION:
    case GL_AMBIENT_AND_DIFFUSE:
    case GL_TEXTURE_BORDER_COLOR:
    case GL_TEXTURE_ENV_COLOR:
    case GL_OBJECT_PLANE:
    case GL_EYE_PLANE:
        return 4;
    case GL_SPOT_DIRECTION:
    case GL_COLOR_INDEXES:
        return 3;
    default:
        return 1;
    }
}

/* How many values glGet returns for a name. */
static unsigned get_count(GLenum name)
{
    switch (name) {
    case GL_MODELVIEW_MATRIX:
    case GL_PROJECTION_MATRIX:
    case GL_TEXTURE_MATRIX:
        return 16;
    case GL_CURRENT_COLOR:
    case GL_CURRENT_TEXTURE_COORDS:
    case GL_CURRENT_RASTER_COLOR:
    case GL_CURRENT_RASTER_TEXTURE_COORDS:
    case GL_CURRENT_RASTER_POSITION:
    case GL_VIEWPORT:
    case GL_SCISSOR_BOX:
    case GL_COLOR_CLEAR_VALUE:
    case GL_COLOR_WRITEMASK:
    case GL_FOG_COLOR:
    case GL_LIGHT_MODEL_AMBIENT:
    case GL_ACCUM_CLEAR_VALUE:
    case GL_MAP2_GRID_DOMAIN:
        return 4;
    case GL_CURRENT_NORMAL:
        return 3;
    case GL_POINT_SIZE_RANGE:
    case GL_LINE_WIDTH_RANGE:
    case GL_POLYGON_MODE:
    case GL_DEPTH_RANGE:
    case GL_MAX_VIEWPORT_DIMS:
    case GL_MAP1_GRID_DOMAIN:
    case GL_MAP2_GRID_SEGMENTS:
        return 2;
    default:
        return 1;
    }
}

/* ---- parameters by pointer -------------------------------------------- */

#define put_int(at, value) (*(at) = (unsigned)(value))

#define VECTOR1(Name, Type, put, width)                                     \
    static void APIENTRY icd_##Name(GLenum pname, const Type *params)       \
    {                                                                       \
        unsigned count = parameter_count(pname), i;                         \
        unsigned *a = entgl_command(ENTGL_OP_##Name, 1 + count * width);    \
        a[0] = pname;                                                       \
        for (i = 0; i < count; i++) {                                       \
            put(a + 1 + i * width, params[i]);                              \
        }                                                                   \
    }

#define VECTOR2(Name, Type, put, width)                                            \
    static void APIENTRY icd_##Name(GLenum target, GLenum pname, const Type *params) \
    {                                                                              \
        unsigned count = parameter_count(pname), i;                                \
        unsigned *a = entgl_command(ENTGL_OP_##Name, 2 + count * width);           \
        a[0] = target;                                                             \
        a[1] = pname;                                                              \
        for (i = 0; i < count; i++) {                                              \
            put(a + 2 + i * width, params[i]);                                     \
        }                                                                          \
    }

VECTOR1(Fogfv, GLfloat, entgl_put_float, 1)
VECTOR1(Fogiv, GLint, put_int, 1)
VECTOR1(LightModelfv, GLfloat, entgl_put_float, 1)
VECTOR1(LightModeliv, GLint, put_int, 1)
VECTOR2(Lightfv, GLfloat, entgl_put_float, 1)
VECTOR2(Lightiv, GLint, put_int, 1)
VECTOR2(Materialfv, GLfloat, entgl_put_float, 1)
VECTOR2(Materialiv, GLint, put_int, 1)
VECTOR2(TexParameterfv, GLfloat, entgl_put_float, 1)
VECTOR2(TexParameteriv, GLint, put_int, 1)
VECTOR2(TexEnvfv, GLfloat, entgl_put_float, 1)
VECTOR2(TexEnviv, GLint, put_int, 1)
VECTOR2(TexGenfv, GLfloat, entgl_put_float, 1)
VECTOR2(TexGeniv, GLint, put_int, 1)
VECTOR2(TexGendv, GLdouble, entgl_put_double, 2)

static void APIENTRY icd_ClipPlane(GLenum plane, const GLdouble *equation)
{
    unsigned *a = entgl_command(ENTGL_OP_ClipPlane, 9);
    int i;
    a[0] = plane;
    for (i = 0; i < 4; i++) {
        entgl_put_double(a + 1 + 2 * i, equation[i]);
    }
}

static void matrix_f(unsigned opcode, const GLfloat *m)
{
    unsigned *a = entgl_command(opcode, 16);
    int i;
    for (i = 0; i < 16; i++) {
        entgl_put_float(a + i, m[i]);
    }
}

static void matrix_d(unsigned opcode, const GLdouble *m)
{
    unsigned *a = entgl_command(opcode, 32);
    int i;
    for (i = 0; i < 16; i++) {
        entgl_put_double(a + 2 * i, m[i]);
    }
}

static void APIENTRY icd_LoadMatrixf(const GLfloat *m)
{
    matrix_f(ENTGL_OP_LoadMatrixf, m);
}

static void APIENTRY icd_MultMatrixf(const GLfloat *m)
{
    matrix_f(ENTGL_OP_MultMatrixf, m);
}

static void APIENTRY icd_LoadMatrixd(const GLdouble *m)
{
    matrix_d(ENTGL_OP_LoadMatrixd, m);
}

static void APIENTRY icd_MultMatrixd(const GLdouble *m)
{
    matrix_d(ENTGL_OP_MultMatrixd, m);
}

static void APIENTRY icd_EdgeFlagv(const GLboolean *flag)
{
    icd_EdgeFlag(*flag);
}

static void APIENTRY icd_Rectdv(const GLdouble *v1, const GLdouble *v2)
{
    icd_Rectd(v1[0], v1[1], v2[0], v2[1]);
}

static void APIENTRY icd_Rectfv(const GLfloat *v1, const GLfloat *v2)
{
    icd_Rectf(v1[0], v1[1], v2[0], v2[1]);
}

static void APIENTRY icd_Rectiv(const GLint *v1, const GLint *v2)
{
    icd_Recti(v1[0], v1[1], v2[0], v2[1]);
}

static void APIENTRY icd_Rectsv(const GLshort *v1, const GLshort *v2)
{
    icd_Rects(v1[0], v1[1], v2[0], v2[1]);
}

static void APIENTRY icd_DrawBuffer(GLenum mode)
{
    entgl_command(ENTGL_OP_DrawBuffer, 1)[0] = mode;
}

static void APIENTRY icd_ReadBuffer(GLenum mode)
{
    entgl_command(ENTGL_OP_ReadBuffer, 1)[0] = mode;
}

/* ---- showing the picture ---------------------------------------------- */

static void present(Context *c)
{
    POINT corner = {0, 0};
    unsigned *a;
    unsigned done;
#ifndef ENTGL_TEST
    /* (The test's window is never shown.) */
    if (c->window == NULL || !IsWindowVisible(c->window) || IsIconic(c->window)) {
        flush(c, NULL, 0);
        return;
    }
#endif
    ClientToScreen(c->window, &corner);
    a = entgl_command(ENTGL_SWAP, 2);
    a[0] = (unsigned)corner.x;
    a[1] = (unsigned)corner.y;
    /* Answered, so the program cannot run ahead of the host. */
    flush(c, &done, sizeof(done));
}

static void APIENTRY icd_Flush(void)
{
    Context *c = current();
    if (c == NULL) {
        return;
    }
    (void)entgl_command(ENTGL_OP_Flush, 0);
    /* With no back buffer, what is drawn is to be seen. */
    if (!c->double_buffered) {
        present(c);
    } else {
        flush(c, NULL, 0);
    }
}

static void APIENTRY icd_Finish(void)
{
    Context *c = current();
    unsigned done;
    if (c == NULL) {
        return;
    }
    ask(ENTGL_OP_Finish, NULL, 0, &done, 1);
    if (!c->double_buffered) {
        present(c);
    }
}

/* ---- questions ---------------------------------------------------------- */

static GLenum APIENTRY icd_GetError(void)
{
    unsigned error;
    ask(ENTGL_OP_GetError, NULL, 0, &error, 1);
    return error;
}

static const GLubyte *APIENTRY icd_GetString(GLenum name)
{
    /* The four strings, fetched once each: they do not change. */
    static unsigned strings[4][64];
    static int fetched[4];
    unsigned which = name - GL_VENDOR;
    if (which > 3) {
        return NULL;
    }
    if (!fetched[which]) {
        if (ask(ENTGL_OP_GetString, &name, 1, strings[which], 63) == 0) {
            return (const GLubyte *)"";
        }
        fetched[which] = 1;
    }
    return (const GLubyte *)strings[which];
}

/* The client's own state, which the host never hears of. */
static int client_get(Context *c, GLenum name, int *value)
{
    const Client *s = &c->client;
    switch (name) {
    case GL_UNPACK_SWAP_BYTES: *value = s->unpack.swap_bytes; break;
    case GL_UNPACK_LSB_FIRST: *value = s->unpack.lsb_first; break;
    case GL_UNPACK_ROW_LENGTH: *value = s->unpack.row_length; break;
    case GL_UNPACK_SKIP_ROWS: *value = s->unpack.skip_rows; break;
    case GL_UNPACK_SKIP_PIXELS: *value = s->unpack.skip_pixels; break;
    case GL_UNPACK_ALIGNMENT: *value = s->unpack.alignment; break;
    case GL_PACK_SWAP_BYTES: *value = s->pack.swap_bytes; break;
    case GL_PACK_LSB_FIRST: *value = s->pack.lsb_first; break;
    case GL_PACK_ROW_LENGTH: *value = s->pack.row_length; break;
    case GL_PACK_SKIP_ROWS: *value = s->pack.skip_rows; break;
    case GL_PACK_SKIP_PIXELS: *value = s->pack.skip_pixels; break;
    case GL_PACK_ALIGNMENT: *value = s->pack.alignment; break;
    case GL_VERTEX_ARRAY: *value = s->vertex.enabled; break;
    case GL_VERTEX_ARRAY_SIZE: *value = s->vertex.size; break;
    case GL_VERTEX_ARRAY_TYPE: *value = (int)s->vertex.type; break;
    case GL_VERTEX_ARRAY_STRIDE: *value = s->vertex.stride; break;
    case GL_NORMAL_ARRAY: *value = s->normal.enabled; break;
    case GL_NORMAL_ARRAY_TYPE: *value = (int)s->normal.type; break;
    case GL_NORMAL_ARRAY_STRIDE: *value = s->normal.stride; break;
    case GL_COLOR_ARRAY: *value = s->color.enabled; break;
    case GL_COLOR_ARRAY_SIZE: *value = s->color.size; break;
    case GL_COLOR_ARRAY_TYPE: *value = (int)s->color.type; break;
    case GL_COLOR_ARRAY_STRIDE: *value = s->color.stride; break;
    case GL_INDEX_ARRAY: *value = s->index.enabled; break;
    case GL_INDEX_ARRAY_TYPE: *value = (int)s->index.type; break;
    case GL_INDEX_ARRAY_STRIDE: *value = s->index.stride; break;
    case GL_TEXTURE_COORD_ARRAY: *value = s->texcoord.enabled; break;
    case GL_TEXTURE_COORD_ARRAY_SIZE: *value = s->texcoord.size; break;
    case GL_TEXTURE_COORD_ARRAY_TYPE: *value = (int)s->texcoord.type; break;
    case GL_TEXTURE_COORD_ARRAY_STRIDE: *value = s->texcoord.stride; break;
    case GL_EDGE_FLAG_ARRAY: *value = s->edge.enabled; break;
    case GL_EDGE_FLAG_ARRAY_STRIDE: *value = s->edge.stride; break;
    case GL_RENDER_MODE: *value = GL_RENDER; break;
    case GL_CLIENT_ATTRIB_STACK_DEPTH: *value = c->stack_depth; break;
    case GL_MAX_CLIENT_ATTRIB_STACK_DEPTH: *value = CLIENT_STACK; break;
    default:
        return 0;
    }
    return 1;
}

static void APIENTRY icd_GetIntegerv(GLenum pname, GLint *params)
{
    Context *c = current();
    unsigned values[16], count = get_count(pname), i;
    if (c == NULL || client_get(c, pname, params)) {
        return;
    }
    ask(ENTGL_OP_GetIntegerv, &pname, 1, values, 16);
    for (i = 0; i < count; i++) {
        params[i] = (GLint)values[i];
    }
}

static void APIENTRY icd_GetFloatv(GLenum pname, GLfloat *params)
{
    Context *c = current();
    unsigned values[16], count = get_count(pname), i;
    int own;
    if (c == NULL) {
        return;
    }
    if (client_get(c, pname, &own)) {
        params[0] = (GLfloat)own;
        return;
    }
    ask(ENTGL_OP_GetFloatv, &pname, 1, values, 16);
    for (i = 0; i < count; i++) {
        params[i] = get_float(values[i]);
    }
}

static void APIENTRY icd_GetDoublev(GLenum pname, GLdouble *params)
{
    Context *c = current();
    unsigned values[32], count = get_count(pname), i;
    int own;
    if (c == NULL) {
        return;
    }
    if (client_get(c, pname, &own)) {
        params[0] = (GLdouble)own;
        return;
    }
    ask(ENTGL_OP_GetDoublev, &pname, 1, values, 32);
    for (i = 0; i < count; i++) {
        params[i] = get_double(values + 2 * i);
    }
}

static void APIENTRY icd_GetBooleanv(GLenum pname, GLboolean *params)
{
    Context *c = current();
    unsigned values[16], count = get_count(pname), i;
    int own;
    if (c == NULL) {
        return;
    }
    if (client_get(c, pname, &own)) {
        params[0] = own != 0;
        return;
    }
    ask(ENTGL_OP_GetBooleanv, &pname, 1, values, 16);
    for (i = 0; i < count; i++) {
        params[i] = values[i] != 0;
    }
}

static GLboolean APIENTRY icd_IsEnabled(GLenum cap)
{
    Context *c = current();
    unsigned enabled;
    int own;
    if (c == NULL) {
        return GL_FALSE;
    }
    if (cap >= GL_VERTEX_ARRAY && cap <= GL_EDGE_FLAG_ARRAY && client_get(c, cap, &own)) {
        return own != 0;
    }
    ask(ENTGL_OP_IsEnabled, &cap, 1, &enabled, 1);
    return enabled != 0;
}

static GLboolean APIENTRY icd_IsList(GLuint list)
{
    unsigned is;
    ask(ENTGL_OP_IsList, &list, 1, &is, 1);
    return is != 0;
}

static GLboolean APIENTRY icd_IsTexture(GLuint texture)
{
    unsigned is;
    ask(ENTGL_OP_IsTexture, &texture, 1, &is, 1);
    return is != 0;
}

static GLuint APIENTRY icd_GenLists(GLsizei range)
{
    unsigned first, argument = (unsigned)range;
    ask(ENTGL_OP_GenLists, &argument, 1, &first, 1);
    return first;
}

static void APIENTRY icd_GenTextures(GLsizei n, GLuint *textures)
{
    while (n > 0) {
        /* 256 names fit an answer on the stack. */
        unsigned names[256], batch = n > 256 ? 256 : (unsigned)n, i;
        ask(ENTGL_OP_GenTextures, &batch, 1, names, batch);
        for (i = 0; i < batch; i++) {
            textures[i] = names[i];
        }
        textures += batch;
        n -= (GLsizei)batch;
    }
}

static void APIENTRY icd_DeleteTextures(GLsizei n, const GLuint *textures)
{
    while (n > 0) {
        unsigned batch = n > 60 ? 60 : (unsigned)n, i;
        unsigned *a = entgl_command(ENTGL_OP_DeleteTextures, batch);
        for (i = 0; i < batch; i++) {
            a[i] = textures[i];
        }
        textures += batch;
        n -= (GLsizei)batch;
    }
}

static GLboolean APIENTRY icd_AreTexturesResident(GLsizei n, const GLuint *textures,
                                                  GLboolean *residences)
{
    GLsizei i;
    (void)textures;
    for (i = 0; i < n; i++) {
        residences[i] = GL_TRUE;
    }
    return GL_TRUE;
}

static void APIENTRY icd_PrioritizeTextures(GLsizei n, const GLuint *textures,
                                            const GLclampf *priorities)
{
    (void)n;
    (void)textures;
    (void)priorities;
}

/* glGetLight and its kin: two names in, up to four values out. */
static void get2(unsigned opcode, GLenum first, GLenum name, unsigned *values)
{
    unsigned args[2];
    args[0] = first;
    args[1] = name;
    ask(opcode, args, 2, values, 4);
}

#define GET2F(Name)                                                              \
    static void APIENTRY icd_##Name(GLenum first, GLenum pname, GLfloat *params) \
    {                                                                            \
        unsigned values[4], count = parameter_count(pname), i;                   \
        get2(ENTGL_OP_##Name, first, pname, values);                             \
        for (i = 0; i < count; i++) {                                            \
            params[i] = get_float(values[i]);                                    \
        }                                                                        \
    }

#define GET2I(Name)                                                            \
    static void APIENTRY icd_##Name(GLenum first, GLenum pname, GLint *params) \
    {                                                                          \
        unsigned values[4], count = parameter_count(pname), i;                 \
        get2(ENTGL_OP_##Name, first, pname, values);                           \
        for (i = 0; i < count; i++) {                                          \
            params[i] = (GLint)values[i];                                      \
        }                                                                      \
    }

GET2F(GetLightfv)
GET2I(GetLightiv)
GET2F(GetMaterialfv)
GET2I(GetMaterialiv)
GET2F(GetTexEnvfv)
GET2I(GetTexEnviv)
GET2F(GetTexGenfv)
GET2I(GetTexGeniv)
GET2F(GetTexParameterfv)
GET2I(GetTexParameteriv)

static void APIENTRY icd_GetTexGendv(GLenum coord, GLenum pname, GLdouble *params)
{
    unsigned values[4], count = parameter_count(pname), i;
    get2(ENTGL_OP_GetTexGenfv, coord, pname, values);
    for (i = 0; i < count; i++) {
        params[i] = get_float(values[i]);
    }
}

static void APIENTRY icd_GetTexLevelParameterfv(GLenum target, GLint level, GLenum pname,
                                                GLfloat *params)
{
    unsigned args[3], values[4];
    args[0] = target;
    args[1] = (unsigned)level;
    args[2] = pname;
    ask(ENTGL_OP_GetTexLevelParameterfv, args, 3, values, 4);
    params[0] = get_float(values[0]);
}

static void APIENTRY icd_GetTexLevelParameteriv(GLenum target, GLint level, GLenum pname,
                                                GLint *params)
{
    unsigned args[3], values[4];
    args[0] = target;
    args[1] = (unsigned)level;
    args[2] = pname;
    ask(ENTGL_OP_GetTexLevelParameteriv, args, 3, values, 4);
    params[0] = (GLint)values[0];
}

/* ---- pixels ------------------------------------------------------------- */

static void pixel_store(PixelStore *store, GLenum base, GLenum name, GLint value)
{
    switch (name - base) {
    case 0: store->swap_bytes = value != 0; break;
    case 1: store->lsb_first = value != 0; break;
    case 2: if (value >= 0) store->row_length = value; break;
    case 3: if (value >= 0) store->skip_rows = value; break;
    case 4: if (value >= 0) store->skip_pixels = value; break;
    case 5:
        if (value == 1 || value == 2 || value == 4 || value == 8) {
            store->alignment = value;
        }
        break;
    default:
        break;
    }
}

static void APIENTRY icd_PixelStorei(GLenum pname, GLint param)
{
    Context *c = current();
    if (c == NULL) {
        return;
    }
    if (pname >= GL_UNPACK_SWAP_BYTES && pname <= GL_UNPACK_ALIGNMENT) {
        pixel_store(&c->client.unpack, GL_UNPACK_SWAP_BYTES, pname, param);
    } else if (pname >= GL_PACK_SWAP_BYTES && pname <= GL_PACK_ALIGNMENT) {
        pixel_store(&c->client.pack, GL_PACK_SWAP_BYTES, pname, param);
    }
}

static void APIENTRY icd_PixelStoref(GLenum pname, GLfloat param)
{
    icd_PixelStorei(pname, (GLint)param);
}

/* How an image of `width` pixels a row lies in the program's memory. */
typedef struct {
    unsigned row_bytes; /* the bytes of a row that are pixels */
    unsigned stride;    /* from one row to the next */
    unsigned skip;      /* from the pointer to the first pixel */
} Layout;

static int layout(const PixelStore *store, GLsizei width, GLenum format, GLenum type, Layout *out)
{
    unsigned pixel = components(format) * type_bytes(type);
    unsigned type_size = type_bytes(type);
    unsigned length = store->row_length > 0 ? (unsigned)store->row_length : (unsigned)width;
    unsigned align = (unsigned)store->alignment;
    if (pixel == 0 || type == GL_DOUBLE || width < 0 || width > 16384 || length > 16384) {
        return 0;
    }
    out->row_bytes = (unsigned)width * pixel;
    out->stride = length * pixel;
    if (type_size < align) {
        out->stride = (out->stride + align - 1) / align * align;
    }
    out->skip = (unsigned)store->skip_rows * out->stride + (unsigned)store->skip_pixels * pixel;
    return 1;
}

static void copy_rows(unsigned char *to, const unsigned char *from, unsigned rows,
                      unsigned row_bytes, unsigned stride)
{
    unsigned y;
    for (y = 0; y < rows; y++) {
        memcpy(to + y * row_bytes, from + y * stride, row_bytes);
    }
}

static unsigned words_of(unsigned bytes)
{
    return (bytes + 3) / 4;
}

/* glTexSubImage2D in as many records as the rows need. */
static void sub_image(Context *c, GLenum target, GLint level, GLint x, GLint y, GLsizei width,
                      GLsizei height, GLenum format, GLenum type, const unsigned char *pixels,
                      const Layout *l)
{
    unsigned per = l->row_bytes ? ONE_RECORD_BYTES / l->row_bytes : 1;
    GLsizei done = 0;
    if (per == 0) {
        per = 1;
    }
    while (done < height) {
        unsigned rows = (unsigned)(height - done) < per ? (unsigned)(height - done) : per;
        unsigned *a = entgl_large(c, ENTGL_OP_TexSubImage2D, 8 + words_of(rows * l->row_bytes));
        if (a == NULL) {
            return;
        }
        a[0] = target;
        a[1] = (unsigned)level;
        a[2] = (unsigned)x;
        a[3] = (unsigned)(y + done);
        a[4] = (unsigned)width;
        a[5] = rows;
        a[6] = format;
        a[7] = type;
        copy_rows((unsigned char *)(a + 8), pixels + l->skip + (unsigned)done * l->stride, rows,
                  l->row_bytes, l->stride);
        done += (GLsizei)rows;
    }
}

static void APIENTRY icd_TexImage2D(GLenum target, GLint level, GLint internalformat,
                                    GLsizei width, GLsizei height, GLint border, GLenum format,
                                    GLenum type, const GLvoid *pixels)
{
    Context *c = current();
    Layout l;
    unsigned *a;
    unsigned bytes = 0;
    int whole;
    if (c == NULL) {
        return;
    }
    if (pixels != NULL) {
        if (height < 0 || height > 16384 || !layout(&c->client.unpack, width, format, type, &l)) {
            return;
        }
        bytes = l.row_bytes * (unsigned)height;
    }
    /* A small image rides in the record; a large one follows in strips. */
    whole = pixels != NULL && bytes <= ONE_RECORD_BYTES;
    a = entgl_large(c, ENTGL_OP_TexImage2D, 9 + (whole ? words_of(bytes) : 0));
    if (a == NULL) {
        return;
    }
    a[0] = target;
    a[1] = (unsigned)level;
    a[2] = (unsigned)internalformat;
    a[3] = (unsigned)width;
    a[4] = (unsigned)height;
    a[5] = (unsigned)border;
    a[6] = format;
    a[7] = type;
    a[8] = (unsigned)whole;
    if (whole) {
        copy_rows((unsigned char *)(a + 9), (const unsigned char *)pixels + l.skip,
                  (unsigned)height, l.row_bytes, l.stride);
    } else if (pixels != NULL) {
        sub_image(c, target, level, 0, 0, width, height, format, type,
                  (const unsigned char *)pixels, &l);
    }
}

static void APIENTRY icd_TexSubImage2D(GLenum target, GLint level, GLint xoffset, GLint yoffset,
                                       GLsizei width, GLsizei height, GLenum format, GLenum type,
                                       const GLvoid *pixels)
{
    Context *c = current();
    Layout l;
    if (c == NULL || pixels == NULL || height < 0 || height > 16384 ||
        !layout(&c->client.unpack, width, format, type, &l)) {
        return;
    }
    sub_image(c, target, level, xoffset, yoffset, width, height, format, type,
              (const unsigned char *)pixels, &l);
}

static void APIENTRY icd_TexImage1D(GLenum target, GLint level, GLint internalformat,
                                    GLsizei width, GLint border, GLenum format, GLenum type,
                                    const GLvoid *pixels)
{
    Context *c = current();
    Layout l;
    unsigned *a;
    if (c == NULL) {
        return;
    }
    if (pixels != NULL && !layout(&c->client.unpack, width, format, type, &l)) {
        return;
    }
    a = entgl_large(c, ENTGL_OP_TexImage1D, 8 + (pixels != NULL ? words_of(l.row_bytes) : 0));
    if (a == NULL) {
        return;
    }
    a[0] = target;
    a[1] = (unsigned)level;
    a[2] = (unsigned)internalformat;
    a[3] = (unsigned)width;
    a[4] = (unsigned)border;
    a[5] = format;
    a[6] = type;
    a[7] = pixels != NULL;
    if (pixels != NULL) {
        memcpy(a + 8, (const unsigned char *)pixels + l.skip, l.row_bytes);
    }
}

static void APIENTRY icd_TexSubImage1D(GLenum target, GLint level, GLint xoffset, GLsizei width,
                                       GLenum format, GLenum type, const GLvoid *pixels)
{
    Context *c = current();
    Layout l;
    unsigned *a;
    if (c == NULL || pixels == NULL || !layout(&c->client.unpack, width, format, type, &l)) {
        return;
    }
    a = entgl_large(c, ENTGL_OP_TexSubImage1D, 6 + words_of(l.row_bytes));
    if (a == NULL) {
        return;
    }
    a[0] = target;
    a[1] = (unsigned)level;
    a[2] = (unsigned)xoffset;
    a[3] = (unsigned)width;
    a[4] = format;
    a[5] = type;
    memcpy(a + 6, (const unsigned char *)pixels + l.skip, l.row_bytes);
}

static void APIENTRY icd_DrawPixels(GLsizei width, GLsizei height, GLenum format, GLenum type,
                                    const GLvoid *pixels)
{
    Context *c = current();
    Layout l;
    unsigned *a;
    if (c == NULL || pixels == NULL || height < 0 || height > 16384 ||
        !layout(&c->client.unpack, width, format, type, &l)) {
        return;
    }
    a = entgl_large(c, ENTGL_OP_DrawPixels, 4 + words_of(l.row_bytes * (unsigned)height));
    if (a == NULL) {
        return;
    }
    a[0] = (unsigned)width;
    a[1] = (unsigned)height;
    a[2] = format;
    a[3] = type;
    copy_rows((unsigned char *)(a + 4), (const unsigned char *)pixels + l.skip, (unsigned)height,
              l.row_bytes, l.stride);
}

static void APIENTRY icd_ReadPixels(GLint x, GLint y, GLsizei width, GLsizei height,
                                    GLenum format, GLenum type, GLvoid *pixels)
{
    Context *c = current();
    Layout l;
    unsigned per;
    GLsizei done = 0;
    unsigned *answer;
    if (c == NULL || pixels == NULL || height < 0 || height > 16384 ||
        !layout(&c->client.pack, width, format, type, &l) || l.row_bytes == 0) {
        return;
    }
    /* Rows come back tightly packed, as many at a time as an answer holds. */
    per = (ONE_RECORD_BYTES - 64) / l.row_bytes;
    if (per == 0) {
        return;
    }
    answer = (unsigned *)HeapAlloc(heap, 0, ONE_RECORD_BYTES);
    if (answer == NULL) {
        return;
    }
    while (done < height) {
        unsigned rows = (unsigned)(height - done) < per ? (unsigned)(height - done) : per;
        unsigned *a = entgl_command(ENTGL_OP_ReadPixels, 6);
        int got;
        a[0] = (unsigned)x;
        a[1] = (unsigned)(y + done);
        a[2] = (unsigned)width;
        a[3] = rows;
        a[4] = format;
        a[5] = type;
        got = flush(c, answer, ONE_RECORD_BYTES);
        if (got < (int)(rows * l.row_bytes)) {
            break;
        }
        {
            unsigned row;
            unsigned char *to = (unsigned char *)pixels + l.skip + (unsigned)done * l.stride;
            for (row = 0; row < rows; row++) {
                memcpy(to + row * l.stride, (unsigned char *)answer + row * l.row_bytes,
                       l.row_bytes);
            }
        }
        done += (GLsizei)rows;
    }
    HeapFree(heap, 0, answer);
}

static void APIENTRY icd_Bitmap(GLsizei width, GLsizei height, GLfloat xorig, GLfloat yorig,
                                GLfloat xmove, GLfloat ymove, const GLubyte *bitmap)
{
    Context *c = current();
    unsigned row = 0, stride = 0, align, bytes = 0;
    unsigned *a;
    if (c == NULL || width < 0 || height < 0 || width > 16384 || height > 16384) {
        return;
    }
    if (bitmap != NULL) {
        unsigned length = c->client.unpack.row_length > 0 ? (unsigned)c->client.unpack.row_length
                                                          : (unsigned)width;
        align = (unsigned)c->client.unpack.alignment;
        row = ((unsigned)width + 7) / 8;
        stride = ((length + 7) / 8 + align - 1) / align * align;
        bytes = row * (unsigned)height;
    }
    a = entgl_large(c, ENTGL_OP_Bitmap, 7 + words_of(bytes));
    if (a == NULL) {
        return;
    }
    a[0] = (unsigned)width;
    a[1] = (unsigned)height;
    entgl_put_float(a + 2, xorig);
    entgl_put_float(a + 3, yorig);
    entgl_put_float(a + 4, xmove);
    entgl_put_float(a + 5, ymove);
    a[6] = bitmap != NULL;
    if (bitmap != NULL) {
        copy_rows((unsigned char *)(a + 7),
                  bitmap + (unsigned)c->client.unpack.skip_rows * stride, (unsigned)height, row,
                  stride);
    }
}

static void APIENTRY icd_PolygonStipple(const GLubyte *mask)
{
    Context *c = current();
    unsigned *a;
    unsigned align, stride;
    if (c == NULL) {
        return;
    }
    align = (unsigned)c->client.unpack.alignment;
    stride = (4 + align - 1) / align * align;
    a = entgl_command(ENTGL_OP_PolygonStipple, 32);
    copy_rows((unsigned char *)a, mask, 32, 4, stride);
}

/* ---- display lists ------------------------------------------------------ */

static void APIENTRY icd_CallLists(GLsizei n, GLenum type, const GLvoid *lists)
{
    Context *c = current();
    unsigned *a;
    GLsizei i;
    if (c == NULL || n <= 0 || n > (1 << 20)) {
        return;
    }
    switch (type) {
    case GL_BYTE:
    case GL_UNSIGNED_BYTE:
    case GL_SHORT:
    case GL_UNSIGNED_SHORT:
    case GL_INT:
    case GL_UNSIGNED_INT:
    case GL_FLOAT:
        break;
    default:
        return;
    }
    a = entgl_large(c, ENTGL_OP_CallLists, (unsigned)n);
    if (a == NULL) {
        return;
    }
    for (i = 0; i < n; i++) {
        switch (type) {
        case GL_BYTE: a[i] = (unsigned)((const GLbyte *)lists)[i]; break;
        case GL_UNSIGNED_BYTE: a[i] = ((const GLubyte *)lists)[i]; break;
        case GL_SHORT: a[i] = (unsigned)((const GLshort *)lists)[i]; break;
        case GL_UNSIGNED_SHORT: a[i] = ((const GLushort *)lists)[i]; break;
        case GL_FLOAT: a[i] = (unsigned)(GLint)((const GLfloat *)lists)[i]; break;
        default: a[i] = ((const GLuint *)lists)[i]; break;
        }
    }
}

/* ---- vertex arrays ------------------------------------------------------- */

static Array *client_array(Context *c, GLenum name)
{
    switch (name) {
    case GL_VERTEX_ARRAY: return &c->client.vertex;
    case GL_NORMAL_ARRAY: return &c->client.normal;
    case GL_COLOR_ARRAY: return &c->client.color;
    case GL_INDEX_ARRAY: return &c->client.index;
    case GL_TEXTURE_COORD_ARRAY: return &c->client.texcoord;
    case GL_EDGE_FLAG_ARRAY: return &c->client.edge;
    default: return NULL;
    }
}

static void APIENTRY icd_EnableClientState(GLenum array)
{
    Context *c = current();
    Array *a = c != NULL ? client_array(c, array) : NULL;
    if (a != NULL) {
        a->enabled = 1;
    }
}

static void APIENTRY icd_DisableClientState(GLenum array)
{
    Context *c = current();
    Array *a = c != NULL ? client_array(c, array) : NULL;
    if (a != NULL) {
        a->enabled = 0;
    }
}

static void set_array(GLenum name, GLint size, GLenum type, GLsizei stride, const GLvoid *pointer)
{
    Context *c = current();
    Array *a = c != NULL ? client_array(c, name) : NULL;
    if (a != NULL && stride >= 0) {
        a->size = size;
        a->type = type;
        a->stride = stride;
        a->pointer = (const unsigned char *)pointer;
    }
}

static void APIENTRY icd_VertexPointer(GLint size, GLenum type, GLsizei stride,
                                       const GLvoid *pointer)
{
    set_array(GL_VERTEX_ARRAY, size, type, stride, pointer);
}

static void APIENTRY icd_NormalPointer(GLenum type, GLsizei stride, const GLvoid *pointer)
{
    set_array(GL_NORMAL_ARRAY, 3, type, stride, pointer);
}

static void APIENTRY icd_ColorPointer(GLint size, GLenum type, GLsizei stride,
                                      const GLvoid *pointer)
{
    set_array(GL_COLOR_ARRAY, size, type, stride, pointer);
}

static void APIENTRY icd_TexCoordPointer(GLint size, GLenum type, GLsizei stride,
                                         const GLvoid *pointer)
{
    set_array(GL_TEXTURE_COORD_ARRAY, size, type, stride, pointer);
}

static void APIENTRY icd_IndexPointer(GLenum type, GLsizei stride, const GLvoid *pointer)
{
    set_array(GL_INDEX_ARRAY, 1, type, stride, pointer);
}

static void APIENTRY icd_EdgeFlagPointer(GLsizei stride, const GLvoid *pointer)
{
    set_array(GL_EDGE_FLAG_ARRAY, 1, GL_UNSIGNED_BYTE, stride, pointer);
}

static void APIENTRY icd_GetPointerv(GLenum pname, GLvoid *params)
{
    Context *c = current();
    const Array *a = NULL;
    if (c == NULL) {
        return;
    }
    switch (pname) {
    case GL_VERTEX_ARRAY_POINTER: a = &c->client.vertex; break;
    case GL_NORMAL_ARRAY_POINTER: a = &c->client.normal; break;
    case GL_COLOR_ARRAY_POINTER: a = &c->client.color; break;
    case GL_INDEX_ARRAY_POINTER: a = &c->client.index; break;
    case GL_TEXTURE_COORD_ARRAY_POINTER: a = &c->client.texcoord; break;
    case GL_EDGE_FLAG_ARRAY_POINTER: a = &c->client.edge; break;
    default: break;
    }
    *(const void **)params = a != NULL ? a->pointer : NULL;
}

/* glInterleavedArrays' formats: which arrays, of what, at what offset. */
static void APIENTRY icd_InterleavedArrays(GLenum format, GLsizei stride, const GLvoid *pointer)
{
    static const struct {
        GLenum format;
        unsigned char texcoord, color, normal, vertex; /* sizes; 0 = absent */
        unsigned char color_bytes;                     /* 1: unsigned bytes */
    } table[] = {
        {GL_V2F, 0, 0, 0, 2, 0},          {GL_V3F, 0, 0, 0, 3, 0},
        {GL_C4UB_V2F, 0, 4, 0, 2, 1},     {GL_C4UB_V3F, 0, 4, 0, 3, 1},
        {GL_C3F_V3F, 0, 3, 0, 3, 0},      {GL_N3F_V3F, 0, 0, 3, 3, 0},
        {GL_C4F_N3F_V3F, 0, 4, 3, 3, 0},  {GL_T2F_V3F, 2, 0, 0, 3, 0},
        {GL_T4F_V4F, 4, 0, 0, 4, 0},      {GL_T2F_C4UB_V3F, 2, 4, 0, 3, 1},
        {GL_T2F_C3F_V3F, 2, 3, 0, 3, 0},  {GL_T2F_N3F_V3F, 2, 0, 3, 3, 0},
        {GL_T2F_C4F_N3F_V3F, 2, 4, 3, 3, 0}, {GL_T4F_C4F_N3F_V4F, 4, 4, 3, 4, 0},
    };
    Context *c = current();
    const unsigned char *base = (const unsigned char *)pointer;
    unsigned i, offset = 0, total;
    if (c == NULL || stride < 0) {
        return;
    }
    for (i = 0; i < sizeof(table) / sizeof(table[0]); i++) {
        if (table[i].format == format) {
            break;
        }
    }
    if (i == sizeof(table) / sizeof(table[0])) {
        return;
    }
    total = table[i].texcoord * 4u + (table[i].color_bytes ? 4u : table[i].color * 4u) +
            table[i].normal * 4u + table[i].vertex * 4u;
    if (stride == 0) {
        stride = (GLsizei)total;
    }
    c->client.edge.enabled = 0;
    c->client.index.enabled = 0;
    c->client.texcoord.enabled = table[i].texcoord != 0;
    if (table[i].texcoord) {
        icd_TexCoordPointer(table[i].texcoord, GL_FLOAT, stride, base + offset);
        offset += table[i].texcoord * 4u;
    }
    c->client.color.enabled = table[i].color != 0;
    if (table[i].color) {
        icd_ColorPointer(table[i].color, table[i].color_bytes ? GL_UNSIGNED_BYTE : GL_FLOAT,
                         stride, base + offset);
        offset += table[i].color_bytes ? 4u : table[i].color * 4u;
    }
    c->client.normal.enabled = table[i].normal != 0;
    if (table[i].normal) {
        icd_NormalPointer(GL_FLOAT, stride, base + offset);
        offset += 12;
    }
    c->client.vertex.enabled = 1;
    icd_VertexPointer(table[i].vertex, GL_FLOAT, stride, base + offset);
}

static unsigned element_bytes(const Array *a)
{
    return (unsigned)a->size * type_bytes(a->type);
}

static unsigned array_words(const Array *a, unsigned count)
{
    return a->enabled ? 3 + words_of(count * element_bytes(a)) : 0;
}

/* One array's `count` elements from `first`, tightly packed. */
static unsigned *put_array(unsigned *at, unsigned kind, const Array *a, unsigned first,
                           unsigned count)
{
    unsigned element = element_bytes(a);
    unsigned stride = a->stride ? (unsigned)a->stride : element;
    unsigned words = words_of(count * element);
    const unsigned char *from = a->pointer + first * stride;
    unsigned char *to = (unsigned char *)(at + 3);
    unsigned i;
    if (!a->enabled) {
        return at;
    }
    at[0] = kind;
    at[1] = (unsigned)a->size;
    at[2] = a->type;
    if (words) {
        at[3 + words - 1] = 0;
    }
    if (stride == element) {
        memcpy(to, from, count * element);
    } else {
        for (i = 0; i < count; i++) {
            memcpy(to + i * element, from + i * stride, element);
        }
    }
    return at + 3 + words;
}

/* The arrays a draw sends, or 0 if they are not something to draw from. */
static unsigned arrays_words(const Client *s, unsigned count)
{
    if (!s->vertex.enabled || s->vertex.pointer == NULL || element_bytes(&s->vertex) == 0 ||
        (s->normal.enabled && element_bytes(&s->normal) == 0) ||
        (s->color.enabled && element_bytes(&s->color) == 0) ||
        (s->texcoord.enabled && element_bytes(&s->texcoord) == 0)) {
        return 0;
    }
    return array_words(&s->vertex, count) + array_words(&s->normal, count) +
           array_words(&s->color, count) + array_words(&s->texcoord, count);
}

static void put_arrays(unsigned *at, const Client *s, unsigned first, unsigned count)
{
    at = put_array(at, 1, &s->vertex, first, count);
    at = put_array(at, 2, &s->normal, first, count);
    at = put_array(at, 3, &s->color, first, count);
    (void)put_array(at, 4, &s->texcoord, first, count);
}

#define MAX_DRAW (1 << 20)

static void APIENTRY icd_DrawArrays(GLenum mode, GLint first, GLsizei count)
{
    Context *c = current();
    unsigned words;
    unsigned *a;
    if (c == NULL || first < 0 || count <= 0 || count > MAX_DRAW) {
        return;
    }
    words = arrays_words(&c->client, (unsigned)count);
    if (words == 0) {
        return;
    }
    a = entgl_large(c, ENTGL_DRAW_ARRAYS, 2 + words);
    if (a == NULL) {
        return;
    }
    a[0] = mode;
    a[1] = (unsigned)count;
    put_arrays(a + 2, &c->client, (unsigned)first, (unsigned)count);
}

static unsigned index_at(GLenum type, const GLvoid *indices, GLsizei i)
{
    switch (type) {
    case GL_UNSIGNED_BYTE: return ((const GLubyte *)indices)[i];
    case GL_UNSIGNED_SHORT: return ((const GLushort *)indices)[i];
    default: return ((const GLuint *)indices)[i];
    }
}

static void APIENTRY icd_DrawElements(GLenum mode, GLsizei count, GLenum type,
                                      const GLvoid *indices)
{
    Context *c = current();
    unsigned low = 0xffffffffu, high = 0, vertices, words;
    unsigned *a;
    GLsizei i;
    if (c == NULL || indices == NULL || count <= 0 || count > MAX_DRAW ||
        (type != GL_UNSIGNED_BYTE && type != GL_UNSIGNED_SHORT && type != GL_UNSIGNED_INT)) {
        return;
    }
    /* Only the vertices the indices reach are sent, renumbered from 0. */
    for (i = 0; i < count; i++) {
        unsigned index = index_at(type, indices, i);
        if (index < low) {
            low = index;
        }
        if (index > high) {
            high = index;
        }
    }
    vertices = high - low + 1;
    if (vertices > MAX_DRAW) {
        return;
    }
    words = arrays_words(&c->client, vertices);
    if (words == 0) {
        return;
    }
    a = entgl_large(c, ENTGL_DRAW_ELEMENTS, 3 + (unsigned)count + words);
    if (a == NULL) {
        return;
    }
    a[0] = mode;
    a[1] = vertices;
    a[2] = (unsigned)count;
    for (i = 0; i < count; i++) {
        a[3 + i] = index_at(type, indices, i) - low;
    }
    put_arrays(a + 3 + count, &c->client, low, vertices);
}

/* glArrayElement, between glBegin and glEnd: the element's values as the
 * immediate-mode calls they stand for. */
static void APIENTRY icd_ArrayElement(GLint i)
{
    Context *c = current();
    const Client *s;
    const unsigned char *p;
    if (c == NULL || i < 0) {
        return;
    }
    s = &c->client;
#define AT(array) \
    ((array).pointer + (unsigned)i * ((array).stride ? (unsigned)(array).stride : element_bytes(&(array))))
    if (s->normal.enabled && s->normal.pointer != NULL) {
        p = AT(s->normal);
        switch (s->normal.type) {
        case GL_BYTE: icd_Normal3bv((const GLbyte *)p); break;
        case GL_SHORT: icd_Normal3sv((const GLshort *)p); break;
        case GL_INT: icd_Normal3iv((const GLint *)p); break;
        case GL_FLOAT: icd_Normal3fv((const GLfloat *)p); break;
        case GL_DOUBLE: icd_Normal3dv((const GLdouble *)p); break;
        default: break;
        }
    }
    if (s->color.enabled && s->color.pointer != NULL) {
        int four = s->color.size == 4;
        p = AT(s->color);
        switch (s->color.type) {
        case GL_BYTE: four ? icd_Color4bv((const GLbyte *)p) : icd_Color3bv((const GLbyte *)p); break;
        case GL_UNSIGNED_BYTE: four ? icd_Color4ubv(p) : icd_Color3ubv(p); break;
        case GL_SHORT: four ? icd_Color4sv((const GLshort *)p) : icd_Color3sv((const GLshort *)p); break;
        case GL_UNSIGNED_SHORT: four ? icd_Color4usv((const GLushort *)p) : icd_Color3usv((const GLushort *)p); break;
        case GL_INT: four ? icd_Color4iv((const GLint *)p) : icd_Color3iv((const GLint *)p); break;
        case GL_UNSIGNED_INT: four ? icd_Color4uiv((const GLuint *)p) : icd_Color3uiv((const GLuint *)p); break;
        case GL_FLOAT: four ? icd_Color4fv((const GLfloat *)p) : icd_Color3fv((const GLfloat *)p); break;
        case GL_DOUBLE: four ? icd_Color4dv((const GLdouble *)p) : icd_Color3dv((const GLdouble *)p); break;
        default: break;
        }
    }
    if (s->texcoord.enabled && s->texcoord.pointer != NULL) {
        p = AT(s->texcoord);
        switch (s->texcoord.type) {
        case GL_SHORT: {
            const GLshort *v = (const GLshort *)p;
            switch (s->texcoord.size) {
            case 1: icd_TexCoord1sv(v); break;
            case 2: icd_TexCoord2sv(v); break;
            case 3: icd_TexCoord3sv(v); break;
            case 4: icd_TexCoord4sv(v); break;
            }
            break;
        }
        case GL_INT: {
            const GLint *v = (const GLint *)p;
            switch (s->texcoord.size) {
            case 1: icd_TexCoord1iv(v); break;
            case 2: icd_TexCoord2iv(v); break;
            case 3: icd_TexCoord3iv(v); break;
            case 4: icd_TexCoord4iv(v); break;
            }
            break;
        }
        case GL_FLOAT: {
            const GLfloat *v = (const GLfloat *)p;
            switch (s->texcoord.size) {
            case 1: icd_TexCoord1fv(v); break;
            case 2: icd_TexCoord2fv(v); break;
            case 3: icd_TexCoord3fv(v); break;
            case 4: icd_TexCoord4fv(v); break;
            }
            break;
        }
        case GL_DOUBLE: {
            const GLdouble *v = (const GLdouble *)p;
            switch (s->texcoord.size) {
            case 1: icd_TexCoord1dv(v); break;
            case 2: icd_TexCoord2dv(v); break;
            case 3: icd_TexCoord3dv(v); break;
            case 4: icd_TexCoord4dv(v); break;
            }
            break;
        }
        default: break;
        }
    }
    if (s->vertex.enabled && s->vertex.pointer != NULL) {
        p = AT(s->vertex);
        switch (s->vertex.type) {
        case GL_SHORT: {
            const GLshort *v = (const GLshort *)p;
            switch (s->vertex.size) {
            case 2: icd_Vertex2sv(v); break;
            case 3: icd_Vertex3sv(v); break;
            case 4: icd_Vertex4sv(v); break;
            }
            break;
        }
        case GL_INT: {
            const GLint *v = (const GLint *)p;
            switch (s->vertex.size) {
            case 2: icd_Vertex2iv(v); break;
            case 3: icd_Vertex3iv(v); break;
            case 4: icd_Vertex4iv(v); break;
            }
            break;
        }
        case GL_FLOAT: {
            const GLfloat *v = (const GLfloat *)p;
            switch (s->vertex.size) {
            case 2: icd_Vertex2fv(v); break;
            case 3: icd_Vertex3fv(v); break;
            case 4: icd_Vertex4fv(v); break;
            }
            break;
        }
        case GL_DOUBLE: {
            const GLdouble *v = (const GLdouble *)p;
            switch (s->vertex.size) {
            case 2: icd_Vertex2dv(v); break;
            case 3: icd_Vertex3dv(v); break;
            case 4: icd_Vertex4dv(v); break;
            }
            break;
        }
        default: break;
        }
    }
#undef AT
}

static void APIENTRY icd_PushClientAttrib(GLbitfield mask)
{
    Context *c = current();
    if (c == NULL || c->stack_depth >= CLIENT_STACK) {
        return;
    }
    c->stack[c->stack_depth] = c->client;
    c->stack_mask[c->stack_depth] = mask;
    c->stack_depth++;
}

static void APIENTRY icd_PopClientAttrib(void)
{
    Context *c = current();
    const Client *saved;
    GLbitfield mask;
    if (c == NULL || c->stack_depth == 0) {
        return;
    }
    c->stack_depth--;
    saved = &c->stack[c->stack_depth];
    mask = c->stack_mask[c->stack_depth];
    if (mask & GL_CLIENT_PIXEL_STORE_BIT) {
        c->client.pack = saved->pack;
        c->client.unpack = saved->unpack;
    }
    if (mask & GL_CLIENT_VERTEX_ARRAY_BIT) {
        c->client.vertex = saved->vertex;
        c->client.normal = saved->normal;
        c->client.color = saved->color;
        c->client.index = saved->index;
        c->client.texcoord = saved->texcoord;
        c->client.edge = saved->edge;
    }
}

/* ---- not here yet -------------------------------------------------------- */

static GLint APIENTRY icd_RenderMode(GLenum mode)
{
    (void)mode;
    return 0;
}

static void APIENTRY icd_FeedbackBuffer(GLsizei size, GLenum type, GLfloat *buffer)
{
    (void)size;
    (void)type;
    (void)buffer;
}

static void APIENTRY icd_SelectBuffer(GLsizei size, GLuint *buffer)
{
    (void)size;
    (void)buffer;
}

static void APIENTRY icd_Map1d(GLenum target, GLdouble u1, GLdouble u2, GLint stride, GLint order,
                               const GLdouble *points)
{
    (void)target; (void)u1; (void)u2; (void)stride; (void)order; (void)points;
}

static void APIENTRY icd_Map1f(GLenum target, GLfloat u1, GLfloat u2, GLint stride, GLint order,
                               const GLfloat *points)
{
    (void)target; (void)u1; (void)u2; (void)stride; (void)order; (void)points;
}

static void APIENTRY icd_Map2d(GLenum target, GLdouble u1, GLdouble u2, GLint ustride,
                               GLint uorder, GLdouble v1, GLdouble v2, GLint vstride,
                               GLint vorder, const GLdouble *points)
{
    (void)target; (void)u1; (void)u2; (void)ustride; (void)uorder;
    (void)v1; (void)v2; (void)vstride; (void)vorder; (void)points;
}

static void APIENTRY icd_Map2f(GLenum target, GLfloat u1, GLfloat u2, GLint ustride, GLint uorder,
                               GLfloat v1, GLfloat v2, GLint vstride, GLint vorder,
                               const GLfloat *points)
{
    (void)target; (void)u1; (void)u2; (void)ustride; (void)uorder;
    (void)v1; (void)v2; (void)vstride; (void)vorder; (void)points;
}

static void APIENTRY icd_PixelMapfv(GLenum map, GLsizei mapsize, const GLfloat *values)
{
    (void)map; (void)mapsize; (void)values;
}

static void APIENTRY icd_PixelMapuiv(GLenum map, GLsizei mapsize, const GLuint *values)
{
    (void)map; (void)mapsize; (void)values;
}

static void APIENTRY icd_PixelMapusv(GLenum map, GLsizei mapsize, const GLushort *values)
{
    (void)map; (void)mapsize; (void)values;
}

static void APIENTRY icd_GetClipPlane(GLenum plane, GLdouble *equation)
{
    (void)plane;
    equation[0] = equation[1] = equation[2] = equation[3] = 0;
}

static void APIENTRY icd_GetMapdv(GLenum target, GLenum query, GLdouble *v)
{
    (void)target; (void)query; (void)v;
}

static void APIENTRY icd_GetMapfv(GLenum target, GLenum query, GLfloat *v)
{
    (void)target; (void)query; (void)v;
}

static void APIENTRY icd_GetMapiv(GLenum target, GLenum query, GLint *v)
{
    (void)target; (void)query; (void)v;
}

static void APIENTRY icd_GetPixelMapfv(GLenum map, GLfloat *values)
{
    (void)map; (void)values;
}

static void APIENTRY icd_GetPixelMapuiv(GLenum map, GLuint *values)
{
    (void)map; (void)values;
}

static void APIENTRY icd_GetPixelMapusv(GLenum map, GLushort *values)
{
    (void)map; (void)values;
}

static void APIENTRY icd_GetPolygonStipple(GLubyte *mask)
{
    (void)mask;
}

static void APIENTRY icd_GetTexImage(GLenum target, GLint level, GLenum format, GLenum type,
                                     GLvoid *pixels)
{
    (void)target; (void)level; (void)format; (void)type; (void)pixels;
}

/* ---- what opengl32.dll calls ---------------------------------------------- */

typedef struct {
    int count;
    PROC table[336];
} DispatchTable;

static DispatchTable dispatch;

/* A control record outside any context: sent on its own. */
static unsigned control(unsigned opcode, unsigned a, unsigned b, unsigned c, unsigned count)
{
    unsigned stream[4], answer = 0;
    stream[0] = opcode | count << 16;
    stream[1] = a;
    stream[2] = b;
    stream[3] = c;
    transact(stream, 1 + count, &answer, sizeof(answer));
    return answer;
}

DHGLRC APIENTRY DrvCreateLayerContext(HDC dc, INT layer)
{
    Context *c;
    int format = GetPixelFormat(dc);
    if (layer != 0) {
        return 0;
    }
    c = (Context *)HeapAlloc(heap, HEAP_ZERO_MEMORY, sizeof(Context));
    if (c == NULL) {
        return 0;
    }
    c->id = control(ENTGL_CONTEXT_CREATE, 0, 0, 0, 1);
    if (c->id == 0) {
        HeapFree(heap, 0, c);
        return 0;
    }
    c->buffer = c->own;
    c->capacity = BUFFER_WORDS;
    c->used = 4;
    c->double_buffered = format < 1 || format > ENTGL_FORMAT_COUNT || ENTGL_FORMAT_DOUBLE(format);
    c->client.pack.alignment = 4;
    c->client.unpack.alignment = 4;
    c->client.vertex.size = 4;
    c->client.vertex.type = GL_FLOAT;
    c->client.normal.size = 3;
    c->client.normal.type = GL_FLOAT;
    c->client.color.size = 4;
    c->client.color.type = GL_FLOAT;
    c->client.index.size = 1;
    c->client.index.type = GL_FLOAT;
    c->client.texcoord.size = 4;
    c->client.texcoord.type = GL_FLOAT;
    c->client.edge.size = 1;
    c->client.edge.type = GL_UNSIGNED_BYTE;
    EnterCriticalSection(&lock);
    c->next = contexts;
    contexts = c;
    LeaveCriticalSection(&lock);
    return (DHGLRC)c;
}

DHGLRC APIENTRY DrvCreateContext(HDC dc)
{
    return DrvCreateLayerContext(dc, 0);
}

BOOL APIENTRY DrvDeleteContext(DHGLRC handle)
{
    Context *c = (Context *)handle, **link;
    if (c == NULL) {
        return FALSE;
    }
    if (current() == c) {
        TlsSetValue(tls, NULL);
    }
    EnterCriticalSection(&lock);
    for (link = &contexts; *link != NULL; link = &(*link)->next) {
        if (*link == c) {
            *link = c->next;
            break;
        }
    }
    LeaveCriticalSection(&lock);
    control(ENTGL_CONTEXT_DESTROY, c->id, 0, 0, 1);
    if (c->big) {
        HeapFree(heap, 0, c->buffer);
    }
    HeapFree(heap, 0, c);
    return TRUE;
}

BOOL APIENTRY DrvCopyContext(DHGLRC source, DHGLRC destination, UINT mask)
{
    Context *from = (Context *)source, *to = (Context *)destination;
    if (from == NULL || to == NULL) {
        return FALSE;
    }
    flush(from, NULL, 0);
    control(ENTGL_CONTEXT_COPY, from->id, to->id, mask, 3);
    return TRUE;
}

BOOL APIENTRY DrvShareLists(DHGLRC first, DHGLRC second)
{
    Context *a = (Context *)first, *b = (Context *)second;
    if (a == NULL || b == NULL) {
        return FALSE;
    }
    return control(ENTGL_CONTEXT_SHARE, a->id, b->id, 0, 2) != 0;
}

/* Makes a context current on this thread; answers with the functions. */
DispatchTable *APIENTRY DrvSetContext(HDC dc, DHGLRC handle, PVOID set_table)
{
    Context *c = (Context *)handle;
    (void)set_table;
    if (c == NULL) {
        return NULL;
    }
    c->dc = dc;
    c->window = WindowFromDC(dc);
    TlsSetValue(tls, c);
    return &dispatch;
}

BOOL APIENTRY DrvReleaseContext(DHGLRC handle)
{
    Context *c = (Context *)handle;
    if (c != NULL) {
        flush(c, NULL, 0);
    }
    TlsSetValue(tls, NULL);
    return TRUE;
}

BOOL APIENTRY DrvSwapBuffers(HDC dc)
{
    Context *c = current();
    if (c == NULL) {
        return TRUE;
    }
    if (WindowFromDC(dc) != c->window) {
        flush(c, NULL, 0);
        return TRUE;
    }
    present(c);
    return TRUE;
}

BOOL APIENTRY DrvSwapLayerBuffers(HDC dc, UINT planes)
{
    return (planes & WGL_SWAP_MAIN_PLANE) ? DrvSwapBuffers(dc) : TRUE;
}

LONG APIENTRY DrvDescribePixelFormat(HDC dc, INT format, ULONG size,
                                     PIXELFORMATDESCRIPTOR *descriptor)
{
    (void)dc;
    if (descriptor != NULL && size >= sizeof(PIXELFORMATDESCRIPTOR) && format >= 1 &&
        format <= ENTGL_FORMAT_COUNT) {
        ENTGL_DESCRIBE(descriptor, format);
    }
    return ENTGL_FORMAT_COUNT;
}

BOOL APIENTRY DrvSetPixelFormat(HDC dc, LONG format)
{
    (void)dc;
    return format >= 1 && format <= ENTGL_FORMAT_COUNT;
}

BOOL APIENTRY DrvValidateVersion(ULONG version)
{
    (void)version;
    return TRUE;
}

void APIENTRY DrvSetCallbackProcs(INT count, PROC *procs)
{
    (void)count;
    (void)procs;
}

/* No extensions: it is OpenGL 1.1. */
PROC APIENTRY DrvGetProcAddress(LPCSTR name)
{
    (void)name;
    return NULL;
}

BOOL APIENTRY DrvDescribeLayerPlane(HDC dc, INT format, INT layer, UINT size,
                                    LPLAYERPLANEDESCRIPTOR descriptor)
{
    (void)dc; (void)format; (void)layer; (void)size; (void)descriptor;
    return FALSE;
}

int APIENTRY DrvGetLayerPaletteEntries(HDC dc, INT layer, INT start, INT count, COLORREF *colors)
{
    (void)dc; (void)layer; (void)start; (void)count; (void)colors;
    return 0;
}

int APIENTRY DrvSetLayerPaletteEntries(HDC dc, INT layer, INT start, INT count,
                                       const COLORREF *colors)
{
    (void)dc; (void)layer; (void)start; (void)count; (void)colors;
    return 0;
}

BOOL APIENTRY DrvRealizeLayerPalette(HDC dc, INT layer, BOOL realize)
{
    (void)dc; (void)layer; (void)realize;
    return FALSE;
}

BOOL WINAPI DllEntry(HINSTANCE instance, DWORD reason, LPVOID reserved)
{
    (void)reserved;
    if (reason == DLL_PROCESS_ATTACH) {
        int i;
        DisableThreadLibraryCalls(instance);
        heap = GetProcessHeap();
        tls = TlsAlloc();
        if (tls == TLS_OUT_OF_INDEXES) {
            return FALSE;
        }
        InitializeCriticalSection(&lock);
        dispatch.count = 336;
        for (i = 0; i < 336; i++) {
            dispatch.table[i] = entgl_dispatch[i];
        }
    } else if (reason == DLL_PROCESS_DETACH) {
        /* A program that leaves without deleting its contexts must not
         * leave them on the host. */
        Context *c;
        for (c = contexts; c != NULL; c = c->next) {
            control(ENTGL_CONTEXT_DESTROY, c->id, 0, 0, 1);
        }
        if (escape_dc != NULL) {
            DeleteDC(escape_dc);
        }
        TlsFree(tls);
    }
    return TRUE;
}
