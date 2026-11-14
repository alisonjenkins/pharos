# Using Pharos with a Kobo (KOReader)

Pharos serves your books as an OPDS catalogue at `https://<your host>/opds/`.
KOReader on a Kobo can browse it, search it and download books from it.

## 1. Make an e-reader password

1. Open Pharos in a browser and sign in.
2. Go to **Settings → Account → E-readers**.
3. Type a name for the reader, such as `Kobo`, and press **Make a password**.
4. Pharos shows the password once, with the catalogue URL and your username.
   Keep the page open until the reader is set up.

The e-reader password opens only the catalogue. Your account password does
not work there, and the e-reader password works nowhere else.

## 2. Add the catalogue in KOReader

1. Open the top menu and tap the **Search** (magnifier) tab.
2. Tap **OPDS catalog**.
3. Tap **+** at the top left.
4. Fill in:
   - **Catalog name:** `Pharos`
   - **Catalog URL:** `https://<your host>/opds/`
   - **Username:** your Pharos username
   - **Password:** the e-reader password. Capitals, spaces and dashes do
     not matter.
5. Tap **Save**, then tap **Pharos** in the list.

## What you see

- **All books**, by title, 50 to a page.
- **Recently added**, newest first.
- **By author**.
- **By library**, when you have more than one Books library.
- **Search**, by title, author or series.

Tap a book, then **EPUB**, **PDF** or **Comic** to download it. A DRM-free
Kindle book downloads as EPUB. Books with Kindle DRM are not listed: they
cannot be opened on the reader.

You see the same books as in Pharos: library access and parental controls
apply.

## When it stops working

- **"Authentication required" or 401:** the password is wrong or revoked.
  Make a new one and edit the catalogue in KOReader.
- **429 or "too many requests":** too many wrong passwords. Wait the time
  the message gives, then try again.
- **403:** your account is outside its allowed hours.

## Revoking a reader

Go to **Settings → Account → E-readers**, tap **Revoke** beside the reader,
then confirm. It stops working on its next request. Revoke a reader you lose
or sell.

## For the operator

- Every check counts `pharos_opds_auth_total{outcome}`. Every catalogue
  request counts `pharos_opds_requests_total{route,outcome}`, and every
  download `pharos_opds_downloads_total{format}`.
- Logs name the username and the outcome, never the password.
- The catalogue relies on TLS at your reverse proxy: Basic auth sends the
  password with every request.
- See ADR-0128 for the design.
