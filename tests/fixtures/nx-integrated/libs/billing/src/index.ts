import { cents } from "@acme/money";

export const bill = (amount: number) => ({ total: cents(amount) });
