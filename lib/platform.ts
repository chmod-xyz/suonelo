import type { Credentials } from "./desktop";

export const isMac = process.env.NEXT_PUBLIC_SUONELO_PLATFORM === "darwin";

export function automaticSetupRetry(token: string): Credentials {
  return isMac
    ? { token, forceDiscovery: true }
    : { token, appId: "", secret: "", forceDiscovery: true };
}
