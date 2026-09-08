#include <asm/unistd.h>
#include <fcntl.h>
#include <linux/kvm.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/mman.h>

enum { CONTEXTS = 2, ROUNDS = 8, PAGE_SIZE = 4096, DATA_GPA = 4096 };

static const uint8_t guest_code[] = {
    0x66, 0x43,                               /* inc %ebx */
    0xba, 0xe9, 0x00,                         /* mov $0xe9, %dx */
    0x66, 0xa1, 0x00, 0x10,                   /* mov 0x1000, %eax */
    0x66, 0xef,                               /* out %eax, (%dx) */
    0x66, 0xed,                               /* in (%dx), %eax */
    0x66, 0x35, 0x5a, 0xa5, 0xc3, 0x3c,       /* xor $0x3cc3a55a, %eax */
    0x66, 0xa3, 0x04, 0x10,                   /* mov %eax, 0x1004 */
    0x66, 0xef,                               /* out %eax, (%dx) */
    0xf4,                                     /* hlt */
};
static uint8_t guest_memory[PAGE_SIZE] __attribute__((aligned(PAGE_SIZE)));
static uint32_t guest_data[CONTEXTS][2][PAGE_SIZE / sizeof(uint32_t)]
    __attribute__((aligned(PAGE_SIZE)));

struct context {
    long vm_fd;
    long vcpu_fd;
    struct kvm_run *run;
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

static int create_context(long kvm_fd, long run_size, unsigned int number,
                          struct context *context)
{
    struct kvm_userspace_memory_region region = {
        .slot = 0,
        .guest_phys_addr = 0,
        .memory_size = sizeof(guest_memory),
        .userspace_addr = (uint64_t)guest_memory,
    };
    struct kvm_sregs sregs;
    struct kvm_regs regs = {
        .rbx = 0x12340000 + number * 0x10000,
        .rcx = 0xabc00000 + number,
        .rsi = 0x11220000 + number,
        .rdi = 0x22330000 + number,
        .rsp = 0x800,
        .rbp = 0x900,
        .r8 = 0x33440000 + number,
        .r9 = 0x44550000 + number,
        .r10 = 0x55660000 + number,
        .r11 = 0x66770000 + number,
        .r12 = 0x77880000 + number,
        .r13 = 0x88990000 + number,
        .r14 = 0x99aa0000 + number,
        .r15 = 0xaabb0000 + number,
        .rip = 0,
        .rflags = 2,
    };

    context->vm_fd = syscall3(__NR_ioctl, kvm_fd, KVM_CREATE_VM, 0);
    if (context->vm_fd < 0) {
        return FAIL("create-vm");
    }
    if (syscall3(__NR_ioctl, context->vm_fd, KVM_SET_TSS_ADDR, 0xfffbd000) < 0) {
        return FAIL("set-tss");
    }
    if (syscall3(__NR_ioctl, context->vm_fd, KVM_SET_USER_MEMORY_REGION,
                 (long)&region) < 0) {
        return FAIL("set-memory");
    }
    context->vcpu_fd = syscall3(__NR_ioctl, context->vm_fd, KVM_CREATE_VCPU, 0);
    if (context->vcpu_fd < 0) {
        return FAIL("create-vcpu");
    }
    context->run = (struct kvm_run *)syscall6(__NR_mmap, 0, run_size,
                                             PROT_READ | PROT_WRITE, MAP_SHARED,
                                             context->vcpu_fd, 0);
    if ((unsigned long)context->run >= (unsigned long)-4095) {
        context->run = NULL;
        return FAIL("mmap-run");
    }
    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_GET_SREGS, (long)&sregs) < 0) {
        return FAIL("get-sregs");
    }
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    sregs.ds.base = 0;
    sregs.ds.selector = 0;
    sregs.cr2 = 0x12345000 + number * PAGE_SIZE;
    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_SET_SREGS, (long)&sregs) < 0) {
        return FAIL("set-sregs");
    }
    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_SET_REGS, (long)&regs) < 0) {
        return FAIL("set-regs");
    }
    return 0;
}

static int run_io(struct context *context, long run_size, uint8_t direction,
                  uint32_t value)
{
    struct kvm_run *run = context->run;
    uint8_t *data;
    size_t index;

    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_RUN, 0) < 0) {
        return FAIL("run");
    }
    if (run->exit_reason != KVM_EXIT_IO || run->io.direction != direction ||
        run->io.port != 0xe9 || run->io.size != sizeof(value) || run->io.count != 1) {
        return FAIL("unexpected-exit");
    }
    if (run->io.data_offset > (uint64_t)run_size ||
        sizeof(value) > (uint64_t)run_size - run->io.data_offset) {
        return FAIL("io-bounds");
    }
    data = (uint8_t *)run + run->io.data_offset;
    for (index = 0; index < sizeof(value); ++index) {
        uint8_t byte = (uint8_t)(value >> (index * 8));

        if (direction == KVM_EXIT_IO_IN) {
            data[index] = byte;
        } else if (data[index] != byte) {
            return FAIL("io-data");
        }
    }
    return 0;
}

static int halt_and_check(struct context *context, unsigned int number,
                          unsigned int round, uint32_t output)
{
    struct kvm_regs regs;
    struct kvm_sregs sregs;

    /* Complete the preceding OUT before examining state or replacing a slot.
     * KVM API section 5: IO is incomplete until the next KVM_RUN invocation.
     * https://www.kernel.org/doc/html/latest/virt/kvm/api.html */
    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_RUN, 0) < 0 ||
        context->run->exit_reason != KVM_EXIT_HLT) {
        return FAIL("complete-io-halt");
    }
    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_GET_REGS, (long)&regs) < 0 ||
        syscall3(__NR_ioctl, context->vcpu_fd, KVM_GET_SREGS, (long)&sregs) < 0) {
        return FAIL("get-completed-state");
    }
    if (regs.rax != output || regs.rbx != 0x12340000 + number * 0x10000 + round + 1 ||
        regs.rcx != 0xabc00000 + number || regs.rdx != 0xe9 ||
        regs.rsi != 0x11220000 + number || regs.rdi != 0x22330000 + number ||
        regs.rsp != 0x800 || regs.rbp != 0x900 ||
        regs.r8 != 0x33440000 + number || regs.r9 != 0x44550000 + number ||
        regs.r10 != 0x55660000 + number || regs.r11 != 0x66770000 + number ||
        regs.r12 != 0x77880000 + number || regs.r13 != 0x88990000 + number ||
        regs.r14 != 0x99aa0000 + number || regs.r15 != 0xaabb0000 + number ||
        regs.rip != sizeof(guest_code) || (regs.rflags & 0x803) != 2 ||
        sregs.cr2 != 0x12345000 + number * PAGE_SIZE ||
        sregs.cs.base != 0 || sregs.cs.selector != 0 ||
        sregs.ds.base != 0 || sregs.ds.selector != 0) {
        return FAIL("completed-state");
    }
    if (round + 1 < ROUNDS) {
        /* Keep the verified GPR/segment state; only rewind the completed code. */
        regs.rip = 0;
        if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_SET_REGS, (long)&regs) < 0) {
            return FAIL("rewind-rip");
        }
    }
    return 0;
}

static int destroy_context(struct context *context, long run_size)
{
    int status = 0;

    /* Try every release even if an earlier operation fails. Process exit is
     * still the cleanup backstop, but never substitutes for a successful gate. */
    if (context->run != NULL &&
        syscall3(__NR_munmap, (long)context->run, run_size, 0) < 0) {
        status = FAIL("munmap-run");
    }
    if (context->vcpu_fd >= 0 && syscall1(__NR_close, context->vcpu_fd) < 0) {
        status = FAIL("close-vcpu");
    }
    if (context->vm_fd >= 0 && syscall1(__NR_close, context->vm_fd) < 0) {
        status = FAIL("close-vm");
    }
    return status;
}

static int probe(void)
{
    static const char success[] = "thin-hv: linux L1 L2 KVM PASS\n";
    struct context contexts[CONTEXTS] = {
        {.vm_fd = -1, .vcpu_fd = -1, .run = NULL},
        {.vm_fd = -1, .vcpu_fd = -1, .run = NULL},
    };
    uint32_t retired[CONTEXTS][2] = {{0}};
    uint32_t input[CONTEXTS];
    uint32_t initial[CONTEXTS];
    long kvm_fd;
    long run_size = 0;
    unsigned int number;
    unsigned int round;
    size_t index;
    int status = 1;

    for (index = 0; index < sizeof(guest_code); ++index) {
        guest_memory[index] = guest_code[index];
    }
    kvm_fd = syscall4(__NR_openat, AT_FDCWD, (long)"/dev/kvm", O_RDWR | O_CLOEXEC, 0);
    if (kvm_fd < 0) {
        return FAIL("open");
    }
    if (syscall3(__NR_ioctl, kvm_fd, KVM_GET_API_VERSION, 0) != KVM_API_VERSION) {
        FAIL("api-version");
        goto out;
    }
    run_size = syscall3(__NR_ioctl, kvm_fd, KVM_GET_VCPU_MMAP_SIZE, 0);
    if (run_size < (long)sizeof(struct kvm_run)) {
        FAIL("run-size");
        goto out;
    }
    for (number = 0; number < CONTEXTS; ++number) {
        if (create_context(kvm_fd, run_size, number, &contexts[number])) {
            goto out;
        }
    }
    /* Two independent single-vCPU VMs, not L2 SMP. Interleave every exit phase
     * to exercise VMCS context changes while one VM has pending userspace IO. */
    for (round = 0; round < ROUNDS; ++round) {
        unsigned int active = round % 2;

        for (number = 0; number < CONTEXTS; ++number) {
            struct kvm_userspace_memory_region region = {
                .slot = 1,
                .guest_phys_addr = DATA_GPA,
                .memory_size = 0,
                .userspace_addr = (uint64_t)guest_data[number][active],
            };

            initial[number] = 0x4b4f324c ^ (number * 0x1020304) ^ (round * 0x10001);
            input[number] = 0x89abcdef ^ (number * 0x1030507) ^ (round * 0x20101);
            guest_data[number][active][0] = initial[number];
            guest_data[number][active][1] = 0;
            /* Delete/recreate avoids assuming that changing userspace_addr of
             * a live slot is supported. Neither VM is in KVM_RUN here, and all
             * IO from the preceding round is complete at KVM_EXIT_HLT. */
            if (round != 0 &&
                syscall3(__NR_ioctl, contexts[number].vm_fd,
                         KVM_SET_USER_MEMORY_REGION, (long)&region) < 0) {
                FAIL("delete-data-slot");
                goto out;
            }
            region.memory_size = PAGE_SIZE;
            if (syscall3(__NR_ioctl, contexts[number].vm_fd,
                         KVM_SET_USER_MEMORY_REGION, (long)&region) < 0) {
                FAIL("create-data-slot");
                goto out;
            }
        }
        for (number = 0; number < CONTEXTS; ++number) {
            if (run_io(&contexts[number], run_size, KVM_EXIT_IO_OUT, initial[number])) {
                goto out;
            }
        }
        for (number = 0; number < CONTEXTS; ++number) {
            if (run_io(&contexts[number], run_size, KVM_EXIT_IO_IN, input[number])) {
                goto out;
            }
        }
        for (number = 0; number < CONTEXTS; ++number) {
            if (run_io(&contexts[number], run_size, KVM_EXIT_IO_OUT,
                       input[number] ^ 0x3cc3a55a)) {
                goto out;
            }
        }
        for (number = 0; number < CONTEXTS; ++number) {
            uint32_t output = input[number] ^ 0x3cc3a55a;

            if (halt_and_check(&contexts[number], number, round, output)) {
                goto out;
            }
            if (guest_data[number][active][0] != initial[number] ||
                guest_data[number][active][1] != output ||
                guest_data[number][active ^ 1][1] != retired[number][active ^ 1]) {
                FAIL("data-slot-coherence");
                goto out;
            }
            retired[number][active] = output;
        }
    }
    status = 0;
out:
    for (number = 0; number < CONTEXTS; ++number) {
        if (destroy_context(&contexts[number], run_size)) {
            status = 1;
        }
    }
    if (syscall1(__NR_close, kvm_fd) < 0) {
        status = FAIL("close-kvm");
    }
    if (status == 0) {
        write_all(success, sizeof(success) - 1);
    }
    return status;
}

/* Linux enters with a 16-byte-aligned stack, not the normal C call-frame
 * alignment. Realign before compiler-generated stack/SIMD operations. */
__attribute__((force_align_arg_pointer, noreturn)) void _start(void)
{
    syscall1(__NR_exit_group, probe());
    __builtin_unreachable();
}
