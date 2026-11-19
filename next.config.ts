import type { NextConfig } from "next";

const config: NextConfig = {
  output: "export",
  images: { unoptimized: true },
  poweredByHeader: false,
  env: {
    NEXT_PUBLIC_SUONELO_PLATFORM:
      process.env.NEXT_PUBLIC_SUONELO_PLATFORM ?? process.platform,
  },
};

export default config;
