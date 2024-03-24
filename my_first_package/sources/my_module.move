module my_first_package::my_module {
    use sui::object::{Self, UID};
    use sui::tx_context::{Self, TxContext};
    use sui::transfer;

    struct Result has key {
        id: UID,
        res: u64,
    }

    public fun add(a: u64, b: u64, ctx: &mut TxContext) {
        let result = Result {
            id: object::new(ctx),
            res: a + b,
        };
        transfer::transfer(result, tx_context::sender(ctx));
    }
}