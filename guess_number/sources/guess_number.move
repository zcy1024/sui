module guess_number::guess_number {
    use sui::object::{Self, ID, UID};
    use sui::tx_context::{Self, TxContext};
    use sui::transfer;
    use sui::clock::{Self, Clock};
    use sui::event;
    use sui::math;

    const VERSION: u64 = 2;

    const ENOTVERSION: u64 = 0;
    const ENOTADMIN: u64 = 1;
    const ENOTUPGRADE: u64 = 2;

    struct Count has key {
        id: UID,
        version: u64,
        admin: ID,
        total: u64,
    }

    struct AdminCap has key {
        id: UID,
    }

    struct Prize has key {
        id: UID,
        prize: u8,
    }

    struct GuessEvent has copy, drop {
        total_count: u64,
        final_winner: address,
    }

    fun init(ctx: &mut TxContext) {
        let admin_cap = AdminCap {id: object::new(ctx)};

        let count = Count {
            id: object::new(ctx),
            version: VERSION,
            admin: object::id(&admin_cap),
            total: 0,
        };

        transfer::transfer(admin_cap, tx_context::sender(ctx));
        transfer::share_object(count);
    }

    fun send_prize(count: u64, prize: u8, ctx: &mut TxContext) {
        transfer::transfer(Prize {
            id: object::new(ctx),
            prize,
        }, tx_context::sender(ctx));

        event::emit(GuessEvent {
            total_count: count,
            final_winner: tx_context::sender(ctx),
        });
    }

    public entry fun guess_between_zero_and_hundred(count: &mut Count, number: u8, clock: &Clock, ctx: &mut TxContext) {
        assert!(count.version == VERSION, ENOTVERSION);

        let des_number = ((clock::timestamp_ms(clock) % 11) as u8);
        if (number == des_number) {
            let prize = (math::min((number as u64) * (count.total + 1) * 10, 255) as u8);
            send_prize(count.total, prize, ctx);
            count.total = 0;
        } else {
            count.total = count.total + 1;
        }
    }

    entry fun migrate(count: &mut Count, admin_cap: &AdminCap) {
        assert!(count.admin == object::id(admin_cap), ENOTADMIN);
        assert!(count.version != VERSION, ENOTUPGRADE);

        count.version = VERSION;
    }
}