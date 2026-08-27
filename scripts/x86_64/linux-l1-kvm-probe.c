#include <asm/unistd.h>
#include <fcntl.h>
#include <linux/kvm.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/mman.h>

static uint8_t guest_memory[4096] __attribute__((aligned(4096))) = {
    0xba, 0xe9, 0x00,                         /* mov $0xe9, %dx */
    0x66, 0xb8, 'L', '2', 'O', 'K',         /* mov $0x4b4f324c, %eax */
    0x66, 0xef,                               /* out %eax, (%dx) */
    0xf4,                                     /* hlt */
};

static long syscall1(long number, long arg1)
{
    long result;

    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "a"(number), "D"(arg1)
                     : "rcx", "r11", "memory");
    return result;
}

static long syscall3(long number, long arg1, long arg2, long arg3)
{
    long result;

    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "a"(number), "D"(arg1), "S"(arg2), "d"(arg3)
                     : "rcx", "r11", "memory");
    return result;
}

static long syscall4(long number, long arg1, long arg2, long arg3, long arg4)
{
    register long r10 __asm__("r10") = arg4;
    long result;

    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "a"(number), "D"(arg1), "S"(arg2), "d"(arg3), "r"(r10)
                     : "rcx", "r11", "memory");
    return result;
}

static long syscall6(long number, long arg1, long arg2, long arg3, long arg4,
                     long arg5, long arg6)
{
    register long r10 __asm__("r10") = arg4;
    register long r8 __asm__("r8") = arg5;
    register long r9 __asm__("r9") = arg6;
    long result;

    __asm__ volatile("syscall"
                     : "=a"(result)
                     : "a"(number), "D"(arg1), "S"(arg2), "d"(arg3), "r"(r10),
                       "r"(r8), "r"(r9)
                     : "rcx", "r11", "memory");
    return result;
}

static void write_all(const char *data, size_t length)
{
    while (length != 0) {
        long written = syscall3(__NR_write, 1, (long)data, (long)length);

        if (written <= 0) {
            return;
        }
        data += written;
        length -= (size_t)written;
    }
}

static int fail(const char *stage, size_t length)
{
    static const char prefix[] = "thin-hv: linux L1 L2 KVM FAIL ";

    write_all(prefix, sizeof(prefix) - 1);
    write_all(stage, length);
    write_all("\n", 1);
    return 1;
}

#define FAIL(stage) fail(stage, sizeof(stage) - 1)

static int probe(void)
{
    static const char success[] = "thin-hv: linux L1 L2 KVM PASS\n";
    static const uint8_t expected[] = {'L', '2', 'O', 'K'};
    struct kvm_userspace_memory_region region = {
        .slot = 0,
        .guest_phys_addr = 0,
        .memory_size = sizeof(guest_memory),
        .userspace_addr = (uint64_t)guest_memory,
    };
    struct kvm_sregs sregs;
    struct kvm_regs regs = {
        .rip = 0,
        .rflags = 2,
    };
    struct kvm_run *run;
    long kvm_fd;
    long vm_fd;
    long vcpu_fd;
    long run_size;
    uint64_t data_size;
    uint8_t *data;
    size_t index;

    kvm_fd = syscall4(__NR_openat, AT_FDCWD, (long)"/dev/kvm",
                      O_RDWR | O_CLOEXEC, 0);
    if (kvm_fd < 0) {
        return FAIL("open");
    }
    if (syscall3(__NR_ioctl, kvm_fd, KVM_GET_API_VERSION, 0) != KVM_API_VERSION) {
        return FAIL("api-version");
    }
    vm_fd = syscall3(__NR_ioctl, kvm_fd, KVM_CREATE_VM, 0);
    if (vm_fd < 0) {
        return FAIL("create-vm");
    }
    if (syscall3(__NR_ioctl, vm_fd, KVM_SET_TSS_ADDR, 0xfffbd000) < 0) {
        return FAIL("set-tss");
    }
    if (syscall3(__NR_ioctl, vm_fd, KVM_SET_USER_MEMORY_REGION, (long)&region) < 0) {
        return FAIL("set-memory");
    }
    vcpu_fd = syscall3(__NR_ioctl, vm_fd, KVM_CREATE_VCPU, 0);
    if (vcpu_fd < 0) {
        return FAIL("create-vcpu");
    }
    run_size = syscall3(__NR_ioctl, kvm_fd, KVM_GET_VCPU_MMAP_SIZE, 0);
    if (run_size < (long)sizeof(*run)) {
        return FAIL("run-size");
    }
    run = (struct kvm_run *)syscall6(__NR_mmap, 0, run_size,
                                    PROT_READ | PROT_WRITE, MAP_SHARED, vcpu_fd, 0);
    if ((unsigned long)run >= (unsigned long)-4095) {
        return FAIL("mmap-run");
    }
    if (syscall3(__NR_ioctl, vcpu_fd, KVM_GET_SREGS, (long)&sregs) < 0) {
        return FAIL("get-sregs");
    }
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    if (syscall3(__NR_ioctl, vcpu_fd, KVM_SET_SREGS, (long)&sregs) < 0) {
        return FAIL("set-sregs");
    }
    if (syscall3(__NR_ioctl, vcpu_fd, KVM_SET_REGS, (long)&regs) < 0) {
        return FAIL("set-regs");
    }
    if (syscall3(__NR_ioctl, vcpu_fd, KVM_RUN, 0) < 0) {
        return FAIL("run");
    }
    if (run->exit_reason != KVM_EXIT_IO || run->io.direction != KVM_EXIT_IO_OUT ||
        run->io.port != 0xe9 || run->io.size != sizeof(expected) || run->io.count != 1) {
        return FAIL("unexpected-exit");
    }
    data_size = (uint64_t)run->io.size * run->io.count;
    if (run->io.data_offset > (uint64_t)run_size ||
        data_size > (uint64_t)run_size - run->io.data_offset) {
        return FAIL("io-bounds");
    }
    data = (uint8_t *)run + run->io.data_offset;
    for (index = 0; index < sizeof(expected); ++index) {
        if (data[index] != expected[index]) {
            return FAIL("io-data");
        }
    }
    write_all(success, sizeof(success) - 1);
    return 0;
}

void _start(void)
{
    syscall1(__NR_exit_group, probe());
    __builtin_unreachable();
}
