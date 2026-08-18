# Minimal RISC-V bare-metal kernel for emulator validation
.section .text
.globl _start
.equ UART_BASE, 0x10000000
.equ CLINT_MSIP, 0x02000000
_start:
    li sp, 0x81FF0000
    la t0, _trap_handler
    csrrw zero, mtvec, t0
    csrw medeleg, zero
    csrw mideleg, zero
    li t0, 0x8
    csrs mstatus, t0
    li t0, 0x88
    csrs mie, t0
    la a3, msg_welcome
    call early_puts
    li t0, CLINT_MSIP
    li t1, 1
    sw t1, 0(t0)
    li a3, 'I'
    call uart_putc
    li a3, 'S'
    call uart_putc
    li a3, 'R'
    call uart_putc
    la a3, msg_ok
    call early_puts
_halt:
    wfi
    j _halt
_trap_handler:
    csrr t0, mcause
    li t1, 0x8000000000000003
    beq t0, t1, _msip_handler
    li a3, 'T'
    call uart_putc
    li a3, '?'
    call uart_putc
    j _halt
_msip_handler:
    li t0, CLINT_MSIP
    sw zero, 0(t0)
    li a3, 'I'
    call uart_putc
    mret
early_puts:
    addi sp, sp, -16
    sd ra, 8(sp)
    sd s0, 0(sp)
    mv s0, a3
1:
    lb a3, 0(s0)
    beqz a3, 2f
    call uart_putc
    addi s0, s0, 1
    j 1b
2:
    ld s0, 0(sp)
    ld ra, 8(sp)
    addi sp, sp, 16
    ret
uart_putc:
    li a5, UART_BASE
1:
    lb a6, 5(a5)
    andi a6, a6, 0x20
    beqz a6, 1b
    sb a3, 0(a5)
    ret
.section .rodata
msg_welcome:
    .string "MiniRV
"
msg_ok:
    .string "OK
"
