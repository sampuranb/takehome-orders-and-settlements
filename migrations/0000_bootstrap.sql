-- Bootstrap migration.
--
-- `sqlx::migrate!("./migrations")` resolves this directory at compile time and
-- fails if it does not exist. Git cannot track an empty directory, so the
-- directory needs at least one committed file for a fresh clone or a Docker
-- build to compile at all.
--
-- The version is 0000 on purpose: Feature 4 adds 001_orders.sql and Feature 6
-- adds 002_payments.sql, and SQLx rejects duplicate numeric versions.
--
-- No schema here. Orders and order items arrive in Feature 4, payments in
-- Feature 6. Better Auth owns users, sessions, and accounts in its own
-- database; this application only ever stores the opaque Better Auth user id.

SELECT 1;
