title: Content-Security-Policy (CSP) header - HTTP | MDN
read: main
charset: UTF-8
links: 154
anchors: 61
---
# Content-Security-Policy (CSP) header

Baseline
Widely available
*

This feature is well established and works across many devices and browser versions. It’s been available across browsers since August 2016.

* Some parts of this feature may have varying levels of support.

- [See full compatibility](#browser_compatibility)
- [Learn more](https://developer.mozilla.org/en-US/docs/Glossary/Baseline/Compatibility)

The HTTP **`Content-Security-Policy`** response header allows website administrators to control resources the user agent is allowed to load for a given page. With a few exceptions, policies mostly involve specifying server origins and script endpoints. This helps guard against [cross-site scripting](https://developer.mozilla.org/en-US/docs/Glossary/Cross-site_scripting) attacks.

Violations may be reported using the [Reporting API](https://developer.mozilla.org/en-US/docs/Web/API/Reporting_API). Reports can be observed in the page for which the policy is being enforced, using a [`ReportingObserver`](https://developer.mozilla.org/en-US/docs/Web/API/ReportingObserver), and sent to server endpoints defined in a [`Reporting-Endpoints`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Reporting-Endpoints) HTTP response header and selected using the CSP [`report-to`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/report-to) directive. For more information see [`CSPViolationReport`](https://developer.mozilla.org/en-US/docs/Web/API/CSPViolationReport).

See the [Content Security Policy (CSP)](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CSP) guide for details about how a CSP is delivered to the browser, what it looks like, along with use cases and deployment strategies.

| Header type | [Response header](https://developer.mozilla.org/en-US/docs/Glossary/Response_header) |
| --- | --- |

## Syntax

http

```http
Content-Security-Policy: <policy-directive>; <policy-directive>
```

where `<policy-directive>` consists of: `<directive> <value>` with no internal punctuation.

## Directives

### Fetch directives

Fetch directives control the locations from which certain resource types may be loaded.

[`child-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/child-src)
  Defines the valid sources for [web workers](https://developer.mozilla.org/en-US/docs/Web/API/Web_Workers_API) and nested browsing contexts loaded using elements such as [`<frame>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/frame) and [`<iframe>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/iframe).

  [Fallback](#fallbacks) for `frame-src` and `worker-src`.

[`connect-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/connect-src)
  Restricts the URLs which can be loaded using script interfaces.

[`default-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/default-src)
  Serves as a fallback for the other [fetch directives](https://developer.mozilla.org/en-US/docs/Glossary/Fetch_directive).

  [Fallback](#fallbacks) for all other fetch directives.

[`fenced-frame-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/fenced-frame-src)
  Specifies valid sources for nested browsing contexts loaded into [`<fencedframe>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/fencedframe) elements.

[`font-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/font-src)
  Specifies valid sources for fonts loaded using [`@font-face`](https://developer.mozilla.org/en-US/docs/Web/CSS/Reference/At-rules/@font-face).

[`frame-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/frame-src)
  Specifies valid sources for nested browsing contexts loaded into elements such as [`<frame>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/frame) and [`<iframe>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/iframe).

[`img-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/img-src)
  Specifies valid sources of images and favicons.

[`manifest-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/manifest-src)
  Specifies valid sources of application manifest files.

[`media-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/media-src)
  Specifies valid sources for loading media using the [`<audio>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/audio), [`<video>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/video) and [`<track>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/track) elements.

[`object-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/object-src)
  Specifies valid sources for the [`<object>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/object) and [`<embed>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/embed) elements.

[`prefetch-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/prefetch-src)
  Specifies valid sources to be prefetched or prerendered.

[`script-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/script-src)
  Specifies valid sources for JavaScript and WebAssembly resources.

  [Fallback](#fallbacks) for `script-src-elem` and `script-src-attr`.

[`script-src-elem`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/script-src-elem)
  Specifies valid sources for JavaScript [`<script>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script) elements.

[`script-src-attr`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/script-src-attr)
  Specifies valid sources for JavaScript inline event handlers.

[`style-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/style-src)
  Specifies valid sources for stylesheets.

  [Fallback](#fallbacks) for `style-src-elem` and `style-src-attr`.

[`style-src-elem`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/style-src-elem)
  Specifies valid sources for stylesheets [`<style>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/style) elements and [`<link>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/link) elements with `rel="stylesheet"`.

[`style-src-attr`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/style-src-attr)
  Specifies valid sources for inline styles applied to individual DOM elements.

[`worker-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/worker-src)
  Specifies valid sources for [`Worker`](https://developer.mozilla.org/en-US/docs/Web/API/Worker), [`SharedWorker`](https://developer.mozilla.org/en-US/docs/Web/API/SharedWorker), or [`ServiceWorker`](https://developer.mozilla.org/en-US/docs/Web/API/ServiceWorker) scripts.

All fetch directives may be specified the single value `'none'`, indicating that the specific resource type should be completely blocked, or as one or more *source expression* values, indicating valid sources for that resource type. See [Fetch directive syntax](#fetch_directive_syntax) for more details.

#### Fallbacks

Some fetch directives function as fallbacks for other more granular directives. This means that if the more granular directive is not specified, then the fallback is used to provide a policy for that resource type.

- `default-src` is a fallback for all other fetch directives.
- `script-src` is a fallback for `script-src-attr` and `script-src-elem`.
- `style-src` is a fallback for `style-src-attr` and `style-src-elem`.
- `child-src` is a fallback for `frame-src` and `worker-src`.

For example:

- If `img-src` is omitted but `default-src` is included, then the policy defined by `default-src` will be applied to images.
- If `script-src-elem` is omitted but `script-src` is included, then the policy defined by `script-src` will be applied to `<script>` elements.
- If `script-src-elem` and `script-src` are both omitted, but `default-src` is included, then the policy defined by `default-src` will be applied to `<script>` elements.

### Document directives

Document directives govern the properties of a document or [worker](https://developer.mozilla.org/en-US/docs/Web/API/Web_Workers_API) environment to which a policy applies.

[`base-uri`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/base-uri)
  Restricts the URLs which can be used in a document's [`<base>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/base) element.

[`sandbox`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/sandbox)
  Enables a sandbox for the requested resource similar to the [`<iframe>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/iframe) [`sandbox`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/iframe#sandbox) attribute.

### Navigation directives

Navigation directives govern to which locations a user can navigate or submit a form, for example.

[`form-action`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/form-action)
  Restricts the URLs which can be used as the target of a form submissions from a given context.

[`frame-ancestors`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/frame-ancestors)
  Specifies valid parents that may embed a page using [`<frame>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/frame), [`<iframe>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/iframe), [`<object>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/object), or [`<embed>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/embed).

### Reporting directives

Reporting directives control the destination URL for CSP violation reports in `Content-Security-Policy` and [`Content-Security-Policy-Report-Only`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy-Report-Only).

[`report-to`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/report-to)
  Provides the browser with a token identifying the reporting endpoint or group of endpoints to send CSP violation information to. The endpoints that the token represents are provided through other HTTP headers, such as [`Reporting-Endpoints`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Reporting-Endpoints) and [`Report-To`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Report-To) .

  **Warning:** This directive is intended to replace [`report-uri`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/report-uri); in browsers that support `report-to`, the `report-uri` directive is ignored. However until `report-to` is broadly supported you should specify both headers as shown (where `endpoint_name` is the name of a separately provided endpoint):

  http

  ```http
  Content-Security-Policy: …; report-uri https://endpoint.example.com; report-to endpoint_name
  ```

### Other directives

[`require-trusted-types-for`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/require-trusted-types-for)
  Enforces [Trusted Types](https://developer.mozilla.org/en-US/docs/Web/API/Trusted_Types_API) at the DOM XSS injection sinks.

[`trusted-types`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/trusted-types)
  Used to specify an allowlist of [Trusted Types](https://developer.mozilla.org/en-US/docs/Web/API/Trusted_Types_API) policies. Trusted Types allows applications to lock down DOM XSS injection sinks to only accept non-spoofable, typed values in place of strings.

[`upgrade-insecure-requests`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/upgrade-insecure-requests)
  Instructs user agents to treat all of a site's insecure URLs (those served over HTTP) as though they have been replaced with secure URLs (those served over HTTPS). This directive is intended for websites with large numbers of insecure legacy URLs that need to be rewritten.

### Deprecated directives

[`block-all-mixed-content`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/block-all-mixed-content)
  Prevents loading any assets using HTTP when the page is loaded using HTTPS.

[`report-uri`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/report-uri)
  Provides the browser with a URL where CSP violation reports should be sent. This has been superseded by the [`report-to`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/report-to) directive.

## Fetch directive syntax

All fetch directives may be specified as one of the following:

- the single value `'none'`, indicating that the specific resource type should be completely blocked
- one or more *source expression* values, indicating valid sources for that resource type.

Each source expression takes one of the forms listed below. Note that not all forms are applicable to all fetch directives: see the documentation for each fetch directive to find out which forms are applicable to it.

The `<host-source>` and `<scheme-source>` formats must be unquoted, and all other formats must be enclosed in single quotes.

### 'nonce-<nonce_value>'

This value consists of the string `nonce-` followed by a [nonce](https://developer.mozilla.org/en-US/docs/Glossary/Nonce) value. The nonce value may use any of the characters from [Base64](https://developer.mozilla.org/en-US/docs/Glossary/Base64#base64_characters) or [URL-safe Base64](https://developer.mozilla.org/en-US/docs/Glossary/Base64#url_and_filename_safe_base64).

This string is a random value that the server generates for every HTTP response. For example:

```
'nonce-416d1177-4d12-4e3b-b7c9-f6c409789fb8'
```

The server can then include the same value as the value of the `nonce` attribute of any [`<script>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script) or [`<style>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/style) resources that they intend to load from the document.

The browser compares the value from the CSP directive against the value in the element attribute, and loads the resource only if they match.

If a directive contains a nonce and `unsafe-inline`, then the browser ignores `unsafe-inline`.

See [Nonces](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CSP#nonces) in the CSP guide for more usage information.

**Note:** Nonce source expressions are only applicable to [`<script>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script) and [`<style>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/style) elements.

### '<hash_algorithm>-<hash_value>'

This value consists of a string identifying a hash algorithm, followed by `-`, followed by a hash value. The hash value may use any of the characters from [Base64](https://developer.mozilla.org/en-US/docs/Glossary/Base64#base64_characters) or [URL-safe Base64](https://developer.mozilla.org/en-US/docs/Glossary/Base64#url_and_filename_safe_base64).

- The hash algorithm identifier must be one of `sha256`, `sha384`, or `sha512`.
- The hash value is the base64-encoded [hash](https://developer.mozilla.org/en-US/docs/Glossary/Hash_function) of a `<script>` or `<style>` resource, calculated using one of the following hash functions: SHA-256, SHA-384, or SHA-512.

For example:

```
'sha256-cd9827ad...'
```

When the browser receives the document, it hashes the contents of any `<script>` and `<style>` elements, compares the result with any hashes in the CSP directive, and loads the resource only if there is a match.

If the element loads an external resource (for example, using the [`src`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script#src) attribute), then the element must also have the [`integrity`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script#integrity) attribute set.

If a directive contains a hash and `unsafe-inline`, then the browser ignores `unsafe-inline`.

See [Hashes](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CSP#hashes) in the CSP guide for more usage information.

**Note:** Hash source expressions are only applicable to [`<script>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script) and [`<style>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/style) elements.

### <host-source>

The [URL](https://developer.mozilla.org/en-US/docs/Web/URI) or IP address of a [host](https://developer.mozilla.org/en-US/docs/Glossary/Host) that is a valid source for the resource.

The scheme, port number, and path are optional.

If the scheme is omitted, the scheme of the document's origin is used.

When matching schemes, secure upgrades are allowed. For example:

- `http://example.com` will also permit resources from `https://example.com`
- `ws://example.org` will also permit resources from `wss://example.org`.

Wildcards (`'*'`) can be used for subdomains, host address, and port number, indicating that all legal values of each are valid. For example:

- `http://*.example.com` permits resources from any subdomain of `example.com`, over HTTP or HTTPS.

Paths that end in `/` match any path they are a prefix of. For example:

- `example.com/api/` will permit resources from `example.com/api/users/new`.

Paths that do not end in `/` are matched exactly. For example:

- `https://example.com/file.js` permits resources from `https://example.com/file.js` but not `https://example.com/file.js/file2.js`.

### <scheme-source>

A [scheme](https://developer.mozilla.org/en-US/docs/Web/URI/Reference/Schemes), such as `https:`. The colon is required.

Secure upgrades are allowed, so:

- `http:` will also permit resources loaded using HTTPS
- `ws:` will also permit resources loaded using WSS.

### 'self'

Resources of the given type may only be loaded from the same [origin](https://developer.mozilla.org/en-US/docs/Glossary/Origin) as the document.

Secure upgrades are allowed. For example:

- If the document is served from `http://example.com`, then a CSP of `'self'` will also permit resources from `https://example.com`.
- If the document is served from `ws://example.org`, then a CSP of `'self'` will also permit resources from `wss://example.org`.

### 'trusted-types-eval'

By default, if a CSP contains a `default-src` or a `script-src` directive, then JavaScript functions which evaluate their arguments as JavaScript are disabled. This includes [`eval()`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/eval), the [`code`](https://developer.mozilla.org/en-US/docs/Web/API/Window/setTimeout#code) argument to [`setTimeout()`](https://developer.mozilla.org/en-US/docs/Web/API/Window/setTimeout), or the [`Function()`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Function/Function) constructor.

The `trusted-types-eval` keyword can be used to undo this protection, but only when [Trusted Types](https://developer.mozilla.org/en-US/docs/Web/API/Trusted_Types_API) are enforced and passed to these functions instead of strings. This allows dynamic evaluation of strings as JavaScript, but only after inputs have been passed through a transformation function before it is injected, which has the chance to [sanitize](https://developer.mozilla.org/en-US/docs/Web/Security/Attacks/XSS#sanitization) the input to remove potentially dangerous markup.

The `trusted-types-eval` must be used instead of [`'unsafe-eval'`](#unsafe-eval) when using these methods with trusted types. This ensures that access to the methods is blocked on browsers that don't support trusted types.

**Note:** Developers should avoid using `trusted-types-eval` or these methods unless absolutely necessary. Trusted types ensure that the input passes through a transformation function — they don't ensure that the transformation makes the input safe (and this can be very hard to get right).

See [`eval()` and similar APIs](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CSP#eval_and_similar_apis) in the CSP guide for more usage information.

### 'unsafe-eval'

By default, if a CSP contains a `default-src` or a `script-src` directive, then JavaScript functions which evaluate their arguments as JavaScript are disabled. This includes [`eval()`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/eval), the [`code`](https://developer.mozilla.org/en-US/docs/Web/API/Window/setTimeout#code) argument to [`setTimeout()`](https://developer.mozilla.org/en-US/docs/Web/API/Window/setTimeout), or the [`Function()`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Function/Function) constructor.

The `unsafe-eval` keyword can be used to undo this protection, allowing dynamic evaluation of strings as JavaScript.

**Warning:** Developers should avoid `'unsafe-eval'`, because it defeats much of the purpose of having a CSP. ['trusted-types-eval'](#trusted-types-eval) provides a "potentially" safer alternative if using these methods is necessary.

See [`eval()` and similar APIs](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CSP#eval_and_similar_apis) in the CSP guide for more usage information.

### 'wasm-unsafe-eval'

By default, if a CSP contains a `default-src` or a `script-src` directive, then a page won't be allowed to compile WebAssembly using functions like [`WebAssembly.compileStreaming()`](https://developer.mozilla.org/en-US/docs/WebAssembly/Reference/JavaScript_interface/compileStreaming_static).

The `wasm-unsafe-eval` keyword can be used to undo this protection. This is a much safer alternative to `'unsafe-eval'`, since it does not enable general evaluation of JavaScript.

### 'unsafe-inline'

By default, if a CSP contains a `default-src` or a `script-src` directive, then inline JavaScript is not allowed to execute. This includes:

- inline `<script>` tags
- inline event handler attributes
- `javascript:` URLs.

Similarly, if a CSP contains `default-src` or a `style-src` directive, then inline CSS will not be loaded, including:

- inline `<style>` tags
- [`style`](https://developer.mozilla.org/en-US/docs/Web/API/HTMLElement/style) attributes.

The `unsafe-inline` keyword can be used to undo this protection, allowing all these forms to be loaded.

**Warning:** Developers should avoid `'unsafe-inline'`, because it defeats much of the purpose of having a CSP.

See [Inline JavaScript](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CSP#inline_javascript) in the CSP guide for more usage information.

### 'unsafe-hashes'

By default, if a CSP contains a `default-src` or a `script-src` directive, then inline event handler attributes like `onclick` and inline `style` attributes are not allowed to execute.

The `'unsafe-hashes'` expression allows the browser to use [hash expressions](#hash_algorithm-hash_value) for inline event handlers and `style` attributes. For example, a CSP might contain a directive like this:

http

```http
script-src 'unsafe-hashes' 'sha256-cd9827ad...'
```

If the hash value matches the hash of an inline event handler attribute value or of a `style` attribute value, then the code will be allowed to execute.

**Warning:** The `'unsafe-hashes'` value is unsafe.

In particular, it enables an attack in which the content of the inline event handler attribute is injected into the document as an inline `<script>` element. Suppose the inline event handler is:

html

```html
<button onclick="transferAllMyMoney()">Transfer all my money</button>
```

If an attacker can inject an inline `<script>` element containing this code, the CSP will allow it to execute automatically.

However, `'unsafe-hashes'` is much safer than `'unsafe-inline'`.

### 'inline-speculation-rules'

By default, if a CSP contains a `default-src` or a `script-src` directive, then inline JavaScript is not allowed to execute. The `'inline-speculation-rules'` allows the browser to load inline `<script>` elements that have a [`type`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script/type) attribute of [`speculationrules`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/script/type/speculationrules).

See the [Speculation Rules API](https://developer.mozilla.org/en-US/docs/Web/API/Speculation_Rules_API) for more information.

### 'strict-dynamic'

The `'strict-dynamic'` keyword makes the trust conferred on a script by a [nonce](#nonce-nonce_value) or a [hash](#hash_algorithm-hash_value) extend to scripts that this script dynamically loads, for example by creating new `<script>` tags using [`Document.createElement()`](https://developer.mozilla.org/en-US/docs/Web/API/Document/createElement) and then inserting them into the document using [`Node.appendChild()`](https://developer.mozilla.org/en-US/docs/Web/API/Node/appendChild).

If this keyword is present in a directive, then the following source expression values are all ignored:

- [<host-source>](#host-source)
- [<scheme-source>](#scheme-source)
- [`'self'`](#self)
- [`'unsafe-inline'`](#unsafe-inline)

See [The `strict-dynamic` keyword](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CSP#the_strict-dynamic_keyword) in the CSP guide for more usage information.

### 'report-sample'

If this expression is included in a directive controlling scripts or styles, and the directive causes the browser to block any inline scripts, inline styles, or event handler attributes, then the [violation report](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CSP#violation_reporting) that the browser generates will contain a [`sample`](https://developer.mozilla.org/en-US/docs/Web/API/CSPViolationReport/sample) property containing the first 40 characters of the blocked resource.

## CSP in workers

[Workers](https://developer.mozilla.org/en-US/docs/Web/API/Worker) are in general *not* governed by the content security policy of the document (or parent worker) that created them. To specify a content security policy for the worker, set a `Content-Security-Policy` response header for the request which requested the worker script itself.

The exception to this is if the worker script's origin is a globally unique identifier (for example, if its URL has a scheme of data or blob). In this case, the worker does inherit the content security policy of the document or worker that created it.

## Multiple content security policies

The CSP mechanism allows multiple policies being specified for a resource, including via the `Content-Security-Policy` header, the [`Content-Security-Policy-Report-Only`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy-Report-Only) header and a [`<meta>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/meta) element.

You can use the `Content-Security-Policy` header more than once, as in the example below. Pay special attention to the [`connect-src`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/connect-src) directive here. Even though the second policy would allow the connection, the first policy contains `connect-src 'none'`. Adding additional policies *can only further restrict* the capabilities of the protected resource, which means that there will be no connection allowed and, as the strictest policy, `connect-src 'none'` is enforced.

http

```http
Content-Security-Policy: default-src 'self' http://example.com;
                          connect-src 'none';
Content-Security-Policy: connect-src http://example.com/;
                          script-src http://example.com/
```

## Examples

### Disable unsafe inline code and only allow HTTPS resources

This HTTP header sets the default policy to only allow resource loading (images, fonts, scripts, etc.) over HTTPS. Because the `unsafe-inline` and `unsafe-eval` directives are not set, inline scripts will be blocked.

http

```http
Content-Security-Policy: default-src https:
```

The same restrictions can be applied using the HTML [`<meta>`](https://developer.mozilla.org/en-US/docs/Web/HTML/Reference/Elements/meta) element.

html

```html
<meta http-equiv="Content-Security-Policy" content="default-src https:" />
```

### Allow inline code and HTTPS resources, but disable plugins

This policy could be used on a pre-existing site that uses too much inline code to fix, to ensure resources are loaded only over HTTPS and disable plugins:

http

```http
Content-Security-Policy: default-src https: 'unsafe-eval' 'unsafe-inline'; object-src 'none'
```

### Report but don't enforce violations when testing

This example sets the same restrictions as the previous example, but using the [`Content-Security-Policy-Report-Only`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy-Report-Only) header and the [`report-to`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/report-to) directive. This approach is used during testing to report violations but not block code from executing.

Endpoints (URLs) to send reports to are defined using the [`Reporting-Endpoints`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Reporting-Endpoints) HTTP response header.

http

```http
Reporting-Endpoints: csp-endpoint="https://example.com/csp-reports"
```

A particular endpoint is then selected as the report target in the CSP policy using the [`report-to`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/report-to) directive.

http

```http
Content-Security-Policy-Report-Only: default-src https:; report-uri /csp-violation-report-url/; report-to csp-endpoint
```

Note that the [`report-uri`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/report-uri) directive is also specified above because `report-to` is not yet broadly supported by browsers.

See [Content Security Policy (CSP) implementation](https://developer.mozilla.org/en-US/docs/Web/Security/Practical_implementation_guides/CSP) for more examples.

## Specifications

Specification
[Content Security Policy Level 3 # csp-header](https://w3c.github.io/webappsec-csp/#csp-header)

## Browser compatibility

## See also

- [`Content-Security-Policy-Report-Only`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy-Report-Only)
- CSP [`report-to`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/report-to) directive
- [`Reporting-Endpoints`](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Reporting-Endpoints)
- [`CSPViolationReport`](https://developer.mozilla.org/en-US/docs/Web/API/CSPViolationReport)
- [Reporting API](https://developer.mozilla.org/en-US/docs/Web/API/Reporting_API).
- [Learn about: Content Security Policy](https://developer.mozilla.org/en-US/docs/Web/HTTP/Guides/CSP)
- [Content Security in WebExtensions](https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/Content_Security_Policy)
- [Adopting a strict policy](https://csp.withgoogle.com/docs/strict-csp.html)
- [CSP Evaluator](https://github.com/google/csp-evaluator) - Evaluate your Content Security Policy

## Help improve MDN

[Learn how to contribute](https://developer.mozilla.org/en-US/docs/MDN/Community/Getting_started)

This page was last modified on Mar 22, 2026 by [MDN contributors](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/Content-Security-Policy/contributors.txt).

[View this page on GitHub](https://github.com/mdn/content/blob/main/files/en-us/web/http/reference/headers/content-security-policy/index.md?plain=1) • [Report a problem with this content](https://github.com/mdn/content/issues/new?template=page-report.yml&mdn-url=https%3A%2F%2Fdeveloper.mozilla.org%2Fen-US%2Fdocs%2FWeb%2FHTTP%2FReference%2FHeaders%2FContent-Security-Policy&metadata=%3C%21--+Do+not+make+changes+below+this+line+--%3E%0A%3Cdetails%3E%0A%3Csummary%3EPage+report+details%3C%2Fsummary%3E%0A%0A*+Folder%3A+%60en-us%2Fweb%2Fhttp%2Freference%2Fheaders%2Fcontent-security-policy%60%0A*+MDN+URL%3A+https%3A%2F%2Fdeveloper.mozilla.org%2Fen-US%2Fdocs%2FWeb%2FHTTP%2FReference%2FHeaders%2FContent-Security-Policy%0A*+GitHub+URL%3A+https%3A%2F%2Fgithub.com%2Fmdn%2Fcontent%2Fblob%2Fmain%2Ffiles%2Fen-us%2Fweb%2Fhttp%2Freference%2Fheaders%2Fcontent-security-policy%2Findex.md%0A*+Last+commit%3A+https%3A%2F%2Fgithub.com%2Fmdn%2Fcontent%2Fcommit%2F6720d579bd658f02c56363805e97e69f93dc79f1%0A*+Document+last+modified%3A+2026-03-22T23%3A36%3A38.000Z%0A%0A%3C%2Fdetails%3E)
