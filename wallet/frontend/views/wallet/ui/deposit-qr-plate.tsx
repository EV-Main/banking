import QRCode from "react-qr-code";

import { Logo } from "@/shared/ui/logo";

// A branded deposit-address QR (Figma `qr`): high error-correction modules in deep navy on a
// white rounded plate, with the EV mark inset in the centre (level H tolerates the occlusion).
// 160px on mobile, 180px on desktop — matching the two frames. `value` is the on-chain address
// string the wallet renders alongside it.
export function DepositQr({ value }: { value: string }) {
  return (
    <div className="relative flex size-40 shrink-0 items-center justify-center rounded-xl border border-border bg-white p-2.5 lg:size-45 lg:rounded-2xl lg:p-3.5">
      {/* Fixed hex, not tokens: the module/quiet-zone contrast is a scanner requirement, so it
          must not follow a palette that can be retuned (or themed light) underneath it. */}
      <QRCode value={value} level="H" size={256} fgColor="#0c1626" bgColor="#ffffff" className="h-full w-full" />
      <span className="absolute flex size-7 items-center justify-center rounded-lg bg-white ring-3 ring-white">
        <Logo className="h-3.5 w-auto text-primary-ink" />
      </span>
    </div>
  );
}
