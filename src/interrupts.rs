use crate::println;
use lazy_static::lazy_static;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

lazy_static! {
    static ref IDT: InterruptDescriptorTable = {
        let mut idt = InterruptDescriptorTable::new();
        idt.breakpoint.set_handler_fn(init_breakpoint);
        idt
    };
}

pub fn init_exceptions() {
    IDT.load()
}

extern "x86-interrupt" fn init_breakpoint(stack_frame: InterruptStackFrame) {
    println!("Breakpoint at: {:#?}", stack_frame);
}

#[test_case]
fn test_breakpoint() {
    init_exceptions();

    x86_64::instructions::interrupts::int3();
}
