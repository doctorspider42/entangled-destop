// Native bundle adapter: the manager finds this executable beside itself.
#include <mach-o/dyld.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    (void)argc;
    char executable[PATH_MAX], base[PATH_MAX], target[PATH_MAX];
    uint32_t capacity = sizeof(executable);
    if (_NSGetExecutablePath(executable, &capacity) || !realpath(executable, base)) {
        perror("Cannot locate Entangled bundle");
        return 1;
    }
    char *slash = strrchr(base, '/');
    if (!slash) return 1;
    *slash = '\0';
    const char *suffixes[] = {"/../Frameworks", "/../Resources/vulkan/icd.d/kosmickrisp.json", "/entangled-engine"};
    const char *variables[] = {"DYLD_LIBRARY_PATH", "VK_DRIVER_FILES"};
    for (unsigned i = 0; i < 3; i++) {
        int length = snprintf(target, sizeof(target), "%s%s", base, suffixes[i]);
        if (length < 0 || (size_t)length >= sizeof(target)) {
            fputs("Entangled bundle path is too long\n", stderr);
            return 1;
        }
        if (i < 2 && setenv(variables[i], target, 1)) {
            perror("Cannot configure bundled Vulkan");
            return 1;
        }
    }
    execv(target, argv);
    perror("Cannot start Entangled engine");
    return 1;
}
