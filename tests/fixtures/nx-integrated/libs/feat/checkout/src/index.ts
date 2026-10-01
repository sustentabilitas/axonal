export const checkout = async () => (await import("../../../billing/src/index")).bill(10);
