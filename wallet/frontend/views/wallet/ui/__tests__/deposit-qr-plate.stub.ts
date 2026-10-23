// Stand-in for `../deposit-qr-plate.tsx` in `deposit-qr.test.ts`: the real plate, behind a
// download the test lands or fails by hand (`fakeDownload("wallet.qr-plate", case)`).

import { downloadFor } from "../../../../shared/__tests__/fake-download.ts";
import { DepositQr } from "../deposit-qr-plate.tsx";

await downloadFor("wallet.qr-plate", new URL(import.meta.url).searchParams.get("case") ?? "");

export { DepositQr };
