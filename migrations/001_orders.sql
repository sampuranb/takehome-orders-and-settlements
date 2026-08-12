-- Orders and their line items.
--
-- Money is stored as BIGINT cents everywhere, never NUMERIC. Postgres BIGINT
-- and Rust i64 have exactly the same range, so a value that survives the
-- checked arithmetic in src/orders.rs is representable here and vice versa.
-- NUMERIC would round-trip through a decimal type and reintroduce the class of
-- bug the cents representation exists to prevent.
--
-- Ownership is a plain TEXT column holding Better Auth's opaque user id. It is
-- deliberately not a UUID and not a foreign key: users live in a different
-- database owned by a different service, and inventing a constraint across that
-- boundary would be a lie the database could not enforce.

CREATE TABLE orders (
    id UUID PRIMARY KEY,

    -- Better Auth's opaque user id. Every read filters on this column; it is
    -- the whole of the tenancy model.
    owner_user_id TEXT NOT NULL,

    customer TEXT NOT NULL,

    -- A calendar date, not a timestamp. "Due on the 14th" is the same day for
    -- everyone looking at the order, which a timestamptz would quietly break.
    due_date DATE NOT NULL,

    -- Computed once in Rust from the line items, with checked arithmetic, and
    -- written in the same transaction as those items. A CHECK constraint cannot
    -- re-derive it, because a CHECK cannot see other rows — so the invariant is
    -- held by writing orders and items together or not at all.
    total_cents BIGINT NOT NULL,

    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),

    CONSTRAINT orders_customer_not_blank CHECK (length(btrim(customer)) > 0),
    CONSTRAINT orders_total_cents_non_negative CHECK (total_cents >= 0)
);

-- The dashboard lists one owner's orders by due date, and every other read is
-- also owner-scoped, so the owner column leads.
CREATE INDEX orders_owner_due_date_idx ON orders (owner_user_id, due_date);

CREATE TABLE order_items (
    id UUID PRIMARY KEY,

    -- Line items carry no owner of their own. They are reachable only through
    -- their order, which means the owner check on the order is the only one
    -- that can be forgotten — and the cascade keeps them from outliving it.
    order_id UUID NOT NULL REFERENCES orders (id) ON DELETE CASCADE,

    -- Explicit ordering. Postgres makes no promise about the order rows come
    -- back in, and "the order they were inserted" is not a thing you can ask
    -- for. Reads sort on this column.
    position INTEGER NOT NULL,

    description TEXT NOT NULL,

    -- BIGINT rather than INTEGER so `quantity * unit_price_cents` is i64 × i64
    -- in Rust with no widening cast to get wrong under review.
    quantity BIGINT NOT NULL,
    unit_price_cents BIGINT NOT NULL,
    line_total_cents BIGINT NOT NULL,

    CONSTRAINT order_items_description_not_blank CHECK (length(btrim(description)) > 0),
    CONSTRAINT order_items_quantity_positive CHECK (quantity >= 1),
    CONSTRAINT order_items_unit_price_non_negative CHECK (unit_price_cents >= 0),
    CONSTRAINT order_items_position_non_negative CHECK (position >= 0),

    -- This one CAN be checked in the database, because it only involves columns
    -- of the same row. It is the arithmetic the application is trusted to do,
    -- re-stated where a bug cannot talk its way past it.
    CONSTRAINT order_items_line_total_matches CHECK (line_total_cents = quantity * unit_price_cents),

    -- Also serves as the (order_id, position) index reads sort on.
    CONSTRAINT order_items_unique_position UNIQUE (order_id, position)
);
