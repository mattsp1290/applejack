/* Gate G4e probe: a 32-bit PE that prints a fixed line and exits 0. */
#include <stdio.h>

int main(void)
{
    puts("hello32 ok");
    fflush(stdout);
    return 0;
}
