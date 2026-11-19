import { isMac } from "@/lib/platform";
import { desktop, errorMessage } from "@/lib/desktop";
import type { Notify } from "./toast";

export function PrivacyPolicy({ notify }: { notify: Notify }) {
  return <div className="privacy-policy">
    <section>
      <h3>Overview</h3>
      <p>Suonelo uses data to operate the application, remember your preferences, and improve the relevance of future searches. The application profile and search ranking history are stored on your device. Suonelo uses the ranking history locally and does not upload it, sell it, or use it for advertising.</p>
    </section>
    <section>
      <h3>Information stored on your device</h3>
      <p>The local application profile holds your settings, bookmarks, and records of saved files. Suonelo uses this information to restore your preferences, organize your library, and verify outputs. {isMac ? "macOS Keychain protects your saved Qobuz credentials." : "Saved Qobuz credentials are encrypted using Windows data protection tied to your Windows user account."} Downloaded media and related files are written to the output location you select.</p>
    </section>
    <section>
      <h3>Search history and personalization</h3>
      <p>Suonelo collects local search ranking history to improve future results. One {isMac ? "local" : "portable"} profile shares that history across account and token changes. It records the credited artist and release identifiers, the highest qualifying activity for each release, and the time of the latest qualifying activity. Qualifying activity includes queued, failed, incomplete, and completed downloads and bookmarks. A successful audio download carries more weight. Suonelo uses this history to order results that already match your query.</p>
      <p>The query you type is held in the search session and sent to Qobuz to retrieve catalog results. The local ranking history stores activity records rather than a log of your typed queries. Suonelo does not send your ranking history, scores, or local file index with a search request.</p>
    </section>
    <section>
      <h3>When Suonelo connects to other services</h3>
      <p>Suonelo sends Qobuz the information required to authenticate your account, search the catalog, resolve selected items, and obtain media and artwork. This can include your search terms, catalog identifiers, and authentication information. Qobuz and its delivery services process those requests under their own terms. With email sign-in, you enter your password on Qobuz’s page inside a private sign-in window. Suonelo receives an authorization code, exchanges it for a session, and uses that session in Qobuz API request headers. See the{" "}
        <a href="https://www.qobuz.com/us-en/discover/legals/privacy" onClick={event => {
          event.preventDefault();
          void desktop.openAboutLink("qobuz-privacy").catch(error => notify(errorMessage(error), "error"));
        }}>Qobuz Privacy Policy</a>.
      </p>
      <p>When lyrics lookup is enabled, Suonelo sends relevant track metadata to LRCLIB, such as the track title, artist, album, and duration when available. The local profile and ranking history are not sent to those providers.</p>
    </section>
    <section>
      <h3>Retention and your choices</h3>
      <p>Settings, bookmarks, file records, and search ranking history remain in your local profile until you remove the relevant data or delete the profile. Use Clear search ranking history in Search to reset personalization. Only new activity rebuilds it. Clearing Downloads or confirming a switch to a different Qobuz account keeps ranking history and saved media.</p>
      <p>You can remove saved credentials in Account settings, remove bookmarks in Bookmarks, and manage saved files at your chosen output location. Removing credentials keeps the last verified Qobuz account ID {isMac ? "in macOS Keychain" : "under Windows protection"} so Suonelo can identify a later account switch. Copies you make of the {isMac ? "local" : "portable"} profile or output folder may retain data. Manage those copies separately. Use Qobuz’s privacy controls for information processed by Qobuz.</p>
    </section>
  </div>;
}
