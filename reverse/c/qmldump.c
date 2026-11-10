/*
 * qmldump.so — LD_PRELOAD shim that dumps xochitl's live QML object tree.
 *
 * Interposes QQmlApplicationEngine::load(QUrl const&). The first argument of a
 * C++ member function is `this`, so we capture the engine pointer directly and
 * then, on a helper thread, call QObject::dumpObjectTree() on it once the UI
 * has finished building.
 *
 * Nothing here is qmldump-specific knowledge: every symbol is looked up with
 * dlsym, so there is no need for Qt headers or import libraries.
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>

#define LOAD_MANGLED "_ZN21QQmlApplicationEngine4loadERK4QUrl"
#define CTOR_MANGLED "_ZN21QQmlApplicationEngineC1EP7QObject"
#define DUMP_MANGLED "_ZN7QObject14dumpObjectTreeEv"

static void *g_engine = NULL;
static volatile int g_scheduled = 0;

static void *dumper(void *arg)
{
    (void)arg;
    sleep(25);

    void *h = dlopen("libQt5Core.so.5", RTLD_LAZY);
    if (!h)
        h = dlopen("/usr/lib/libQt5Core.so.5", RTLD_LAZY);
    void (*dump)(void *) = h ? (void (*)(void *))dlsym(h, DUMP_MANGLED) : NULL;

    fprintf(stderr, "[qmldump] engine=%p core=%p dump=%p\n", g_engine, h, dump);
    if (dump && g_engine) {
        fprintf(stderr, "[qmldump] ===== TREE BEGIN =====\n");
        dump(g_engine);
        fprintf(stderr, "[qmldump] ===== TREE END =====\n");
    }
    fflush(stderr);
    return NULL;
}

/* The symbol xochitl imports from libQt5Qml. Naming the C function after the
 * Itanium-mangled name makes it a drop-in interposer. */
void _ZN21QQmlApplicationEngine4loadERK4QUrl(void *self, const void *url)
{
    static void (*real)(void *, const void *) = NULL;
    if (!real)
        real = (void (*)(void *, const void *))dlsym(RTLD_NEXT, LOAD_MANGLED);

    g_engine = self;
    fprintf(stderr, "[qmldump] load() intercepted, engine=%p real=%p\n", self, real);

    if (real)
        real(self, url);

    if (!g_scheduled) {
        g_scheduled = 1;
        pthread_t t;
        if (pthread_create(&t, NULL, dumper, NULL) == 0)
            pthread_detach(t);
    }
}

/* The engine constructor is guaranteed to run at startup, so it is the
 * reliable trigger; load() only refines the timing. */
void _ZN21QQmlApplicationEngineC1EP7QObject(void *self, void *parent)
{
    static void (*real)(void *, void *) = NULL;
    if (!real)
        real = (void (*)(void *, void *))dlsym(RTLD_NEXT, CTOR_MANGLED);

    fprintf(stderr, "[qmldump] ctor() intercepted, engine=%p real=%p\n", self, real);
    if (real)
        real(self, parent);

    g_engine = self;
    if (!g_scheduled) {
        g_scheduled = 1;
        pthread_t t;
        if (pthread_create(&t, NULL, dumper, NULL) == 0)
            pthread_detach(t);
        fprintf(stderr, "[qmldump] dumper thread scheduled\n");
    }
}

__attribute__((constructor)) static void init(void)
{
    /* Keep the dump out of the journal: write it to a file we can scp back. */
    freopen("/tmp/qmldump.txt", "w", stderr);
    fprintf(stderr, "[qmldump] preloaded pid=%d\n", (int)getpid());
}
