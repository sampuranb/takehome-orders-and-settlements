-- Payments recorded against an order.
--
-- The invariant this table exists to protect is that the payments against an
-- order never exceed its total. That invariant spans rows, so no CHECK can hold
-- it: a CHECK sees one row, and the sum it would have to compare against lives
-- in every other row plus the parent order. It is held instead by the write
-- path in src/payments.rs, which locks the order row with FOR UPDATE before it
-- reads the sum, so a second payment waits and then recalculates against a sum
-- that already includes the first.
--
-- The constraints below are the ones a single row CAN answer for. They are not
-- the overpayment rule and are not a substitute for it.

CREATE TABLE payments (
    id UUID PRIMARY KEY,

    -- Payments carry no owner of their own, for the same reason line items do
    -- not: they are reachable only through their order, so the owner check on
    -- the order is the only one that can be forgotten. The cascade keeps them
    -- from outliving the order they settle.
    order_id UUID NOT NULL REFERENCES orders (id) ON DELETE CASCADE,

    -- Strictly positive. A zero payment is not a payment, and a negative one is
    -- a refund — a different operation, with different rules, that this table
    -- deliberately cannot express as a payment.
    amount_cents BIGINT NOT NULL,

    -- The day the money moved, as a calendar date, matching due_date on orders.
    -- Separate from created_at: recording last week's cheque today is normal,
    -- and the two dates answer different questions.
    paid_on DATE NOT NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT payments_amount_positive CHECK (amount_cents > 0)
);

-- Every read of this table is "the payments for this order", either to sum them
-- during a write or to list them on the detail page. The list is newest first,
-- and ids are UUID v7, so ordering on (paid_on, id) is both the index's job and
-- a deterministic tiebreak between two payments made on the same day.
CREATE INDEX payments_order_paid_on_idx ON payments (order_id, paid_on DESC, id DESC);
