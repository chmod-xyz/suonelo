import { desktop, errorMessage } from "@/lib/desktop";
import type { Notify } from "./toast";

export function About({ notify }: { notify: Notify }) {
  const link = (label: string, href: string, id: "project") => (
    <a href={href} onClick={event => {
      event.preventDefault();
      void desktop.openAboutLink(id).catch(error => notify(errorMessage(error), "error"));
    }}>{label}</a>
  );
  return (
    <>
      <div className="original-project-credit">
        <p>Thank you, <strong>HongYue1</strong>, for creating qobuz-cli and sharing your work. The original project inspired suonelo.</p>
        {link("View HongYue1/qobuz-cli on GitHub", "https://github.com/HongYue1/qobuz-cli", "project")}
      </div>
      <div className="about-copy">
        <p>suonelo is an independent community project. It is not affiliated with, sponsored by, endorsed by, or officially connected with Qobuz or XANDRIE SA. References to Qobuz, XANDRIE SA, their services, product names, trademarks, and catalog are used to identify the services with which this application interacts. Those names and marks remain the property of their respective owners.</p>
        <p>This application does not grant ownership of, a license to, or permission to redistribute music, artwork, booklets, lyrics, or other catalog content. Access to media depends on the user&apos;s account, subscription, territory, available formats, and the applicable service terms. Users remain responsible for obtaining the rights and permissions appropriate to their use of downloaded content. Availability, metadata, and service behavior may change independently of this application.</p>
        <p>The project is provided under its applicable software license and without a promise that every service feature will remain available. Software licensing is separate from the rights covering third-party media. No catalog artwork or trademark is claimed as the property of this project. Thank you to HongYue1 for creating qobuz-cli, whose work inspired suonelo.</p>
        <h3>Licenses and acknowledgments</h3>
        <ul>
          <li>qobuz-cli source. GNU GPL version 3 or later. The parent project declares GPL-3.0-or-later.</li>
          <li>Lucide icons. ISC License. Feather-derived icons also retain their MIT License notice.</li>
          <li>Space Grotesk font. SIL Open Font License 1.1.</li>
          <li>React, Next.js, Radix, dnd-kit and Tailwind. MIT License.</li>
          <li>Qobuz-sourced media and artwork remain subject to the rights of their respective owners.</li>
        </ul>
      </div>
    </>
  );
}
