#include <asm/kvm_para.h>
#include <asm/unistd.h>
#include <fcntl.h>
#include <linux/kvm.h>
#include <stddef.h>
#include <stdint.h>
#include <sys/mman.h>

enum {
    CONTEXTS = 2, ROUNDS = 8, PAGE_SIZE = 4096, DATA_GPA = 4096,
    CPUID_ENTRIES = 256, SSE_REGISTERS = 8, SSE_INITIALIZER_SIZE = 53,
};

static struct {
    uint32_t nent;
    uint32_t padding;
    struct kvm_cpuid_entry2 entries[CPUID_ENTRIES];
} supported_cpuid;
_Static_assert(offsetof(struct kvm_cpuid2, entries) ==
               offsetof(typeof(supported_cpuid), entries), "KVM CPUID header layout");

static const uint8_t guest_code[] = {
    0x0f, 0xae, 0x16, 0x08, 0x10,             /* ldmxcsr 0x1008; first round only */
    0xf3, 0x0f, 0x6f, 0x06, 0x00, 0x11,       /* movdqu 0x1100, %xmm0 */
    0xf3, 0x0f, 0x6f, 0x0e, 0x10, 0x11,       /* movdqu 0x1110, %xmm1 */
    0xf3, 0x0f, 0x6f, 0x16, 0x20, 0x11,       /* movdqu 0x1120, %xmm2 */
    0xf3, 0x0f, 0x6f, 0x1e, 0x30, 0x11,       /* movdqu 0x1130, %xmm3 */
    0xf3, 0x0f, 0x6f, 0x26, 0x40, 0x11,       /* movdqu 0x1140, %xmm4 */
    0xf3, 0x0f, 0x6f, 0x2e, 0x50, 0x11,       /* movdqu 0x1150, %xmm5 */
    0xf3, 0x0f, 0x6f, 0x36, 0x60, 0x11,       /* movdqu 0x1160, %xmm6 */
    0xf3, 0x0f, 0x6f, 0x3e, 0x70, 0x11,       /* movdqu 0x1170, %xmm7 */
    0x66, 0x0f, 0xef, 0xc1,                   /* pxor %xmm1, %xmm0 */
    0xf3, 0x0f, 0x7f, 0x06, 0x10, 0x10,       /* movdqu %xmm0, 0x1010 */
    0x0f, 0xae, 0x1e, 0x0c, 0x10,             /* stmxcsr 0x100c */
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

static void write_hex_field(const char *name, size_t length, uint32_t value)
{
    static const char hex[] = "0123456789abcdef";
    char digits[8];
    unsigned int index;

    for (index = 0; index < 8; ++index) {
        digits[index] = hex[(value >> (28 - index * 4)) & 0xf];
    }
    write_all(name, length);
    write_all(digits, sizeof(digits));
}

#define HEX_FIELD(name, value) write_hex_field(name, sizeof(name) - 1, value)

static int fail_value(const char *stage, size_t length, uint32_t actual, uint32_t expected)
{
    int status = fail(stage, length);

    HEX_FIELD(" actual=0x", actual);
    HEX_FIELD(" expected=0x", expected);
    write_all("\n", 1);
    return status;
}

#define FAIL_VALUE(stage, actual, expected) \
    fail_value(stage, sizeof(stage) - 1, actual, expected)

static int initialize_cpuid(long kvm_fd)
{
    unsigned int index;
    unsigned int sse_entries = 0;

    if (syscall3(__NR_ioctl, kvm_fd, KVM_CHECK_EXTENSION, KVM_CAP_EXT_CPUID) <= 0) {
        return FAIL("cpuid-capability");
    }
    supported_cpuid.nent = CPUID_ENTRIES;
    if (syscall3(__NR_ioctl, kvm_fd, KVM_GET_SUPPORTED_CPUID,
                 (long)&supported_cpuid) < 0) {
        return FAIL("get-supported-cpuid");
    }
    if (supported_cpuid.nent == 0 || supported_cpuid.nent > CPUID_ENTRIES) {
        return FAIL("supported-cpuid-count");
    }
    for (index = 0; index < supported_cpuid.nent; ++index) {
        struct kvm_cpuid_entry2 *entry = &supported_cpuid.entries[index];

        if (entry->function == 1 && entry->index == 0) {
            const uint32_t required = (1 << 24) | (1 << 25) | (1 << 26);

            if ((entry->edx & required) != required) {
                return FAIL("cpuid-missing-fxsave-sse2");
            }
            ++sse_entries;
            /* No in-kernel irqchip: exclude x2APIC and TSC-deadline as required
             * by KVM API section 9.1 before passing supported CPUID to SET. */
            entry->ecx &= ~((1U << 21) | (1U << 24));
        } else if (entry->function == KVM_CPUID_FEATURES) {
            entry->eax &= ~(1U << KVM_FEATURE_PV_UNHALT);
        }
    }
    return sse_entries == 1 ? 0 : FAIL("cpuid-sse-leaf-count");
}

static uint8_t xmm_seed(unsigned int number, unsigned int reg, unsigned int byte)
{
    return (uint8_t)(0x31 + number * 0x61 + reg * 0x0d + byte * 7);
}

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
    unsigned int reg;
    unsigned int byte;

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
    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_SET_CPUID2,
                 (long)&supported_cpuid) < 0) {
        return FAIL("set-cpuid");
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
    sregs.cr0 &= ~((uint64_t)(1 << 2) | (1 << 3)); /* EM=TS=0 */
    sregs.cr4 |= 1 << 9;                          /* OSFXSR=1 */
    sregs.cr2 = 0x12345000 + number * PAGE_SIZE;
    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_SET_SREGS, (long)&sregs) < 0) {
        return FAIL("set-sregs");
    }
    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_SET_REGS, (long)&regs) < 0) {
        return FAIL("set-regs");
    }
    /* Initialize SSE in actual guest instructions once, not KVM_SET_FPU:
     * Linux v7.1.5 copies its XMM bytes but does not set SSE's XSTATE_BV bit,
     * so XRSTOR can discard that image as init state on the first KVM_RUN.
     * Real mode exposes XMM0-7; XMM8-15, AVX and full XSAVE are not tested. */
    for (reg = 0; reg < SSE_REGISTERS; ++reg) {
        for (byte = 0; byte < 16; ++byte) {
            ((uint8_t *)guest_data[number][0])[256 + reg * 16 + byte] =
                xmm_seed(number, reg, byte);
        }
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
    struct kvm_fpu fpu;
    const uint8_t *stored_xmm = (const uint8_t *)guest_data[number][round % 2] + 16;
    unsigned int reg;
    unsigned int byte;

    /* Complete the preceding OUT before examining state or replacing a slot.
     * KVM API section 5: IO is incomplete until the next KVM_RUN invocation.
     * https://www.kernel.org/doc/html/latest/virt/kvm/api.html */
    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_RUN, 0) < 0 ||
        context->run->exit_reason != KVM_EXIT_HLT) {
        return FAIL("complete-io-halt");
    }
    if (syscall3(__NR_ioctl, context->vcpu_fd, KVM_GET_REGS, (long)&regs) < 0 ||
        syscall3(__NR_ioctl, context->vcpu_fd, KVM_GET_SREGS, (long)&sregs) < 0 ||
        syscall3(__NR_ioctl, context->vcpu_fd, KVM_GET_FPU, (long)&fpu) < 0) {
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
        (sregs.cr0 & ((1 << 2) | (1 << 3))) != 0 || (sregs.cr4 & (1 << 9)) == 0 ||
        sregs.cr2 != 0x12345000 + number * PAGE_SIZE ||
        sregs.cs.base != 0 || sregs.cs.selector != 0 ||
        sregs.ds.base != 0 || sregs.ds.selector != 0) {
        return FAIL("completed-state");
    }
    /* Linux v7.1.5 get/set_fpu copy XMM but do not copy the mxcsr member.
     * Read the actual guest STMXCSR result instead. Initial LDMXCSR runs once;
     * later rounds must preserve its value across both VMs and userspace exits.
     * https://github.com/gregkh/linux/blob/v7.1.5/arch/x86/kvm/x86.c */
    if (guest_data[number][round % 2][3] != (0x1f80 | (number << 13))) {
        return FAIL_VALUE("sse-mxcsr-state", guest_data[number][round % 2][3],
                          0x1f80 | (number << 13));
    }
    for (reg = 0; reg < SSE_REGISTERS; ++reg) {
        for (byte = 0; byte < 16; ++byte) {
            uint8_t expected = xmm_seed(number, reg, byte);

            if (reg == 0 && round % 2 == 0) {
                expected ^= xmm_seed(number, 1, byte);
            }
            if (fpu.xmm[reg][byte] != expected) {
                int status = FAIL_VALUE("sse-xmm-state", fpu.xmm[reg][byte], expected);

                HEX_FIELD(" vm=0x", number);
                HEX_FIELD(" round=0x", round);
                HEX_FIELD(" xmm=0x", reg);
                HEX_FIELD(" byte=0x", byte);
                HEX_FIELD(" stored_xmm0_byte=0x", stored_xmm[byte]);
                write_all("\n", 1);
                return status;
            }
            if (reg == 0 && stored_xmm[byte] != expected) {
                return FAIL("sse-guest-store");
            }
        }
    }
    if (round + 1 < ROUNDS) {
        /* Retain GPR/segment/FPU state and skip all one-time SSE loads. */
        regs.rip = SSE_INITIALIZER_SIZE;
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
    if (initialize_cpuid(kvm_fd)) {
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
            guest_data[number][active][2] = 0x1f80 | (number << 13);
            guest_data[number][active][3] = 0;
            for (index = 4; index < 8; ++index) {
                guest_data[number][active][index] = 0;
            }
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
