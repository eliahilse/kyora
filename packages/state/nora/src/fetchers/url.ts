import type { FetchedDoc } from "./npm"

export async function fetchUrl(url: string): Promise<FetchedDoc[]> {
  const res = await fetch(url)
  if (!res.ok) throw new Error(`url: failed to fetch ${url}: ${res.status}`)

  const contentType = res.headers.get("content-type") ?? ""
  const content = await res.text()

  // strip html tags for basic html pages
  const cleaned = contentType.includes("text/html") ? stripHtml(content) : content

  return [{
    content: cleaned,
    source: url,
    metadata: { type: "url", contentType },
  }]
}

function stripHtml(html: string): string {
  return html
    .replace(/<script[^>]*>[\s\S]*?<\/script>/gi, "")
    .replace(/<style[^>]*>[\s\S]*?<\/style>/gi, "")
    .replace(/<nav[^>]*>[\s\S]*?<\/nav>/gi, "")
    .replace(/<footer[^>]*>[\s\S]*?<\/footer>/gi, "")
    .replace(/<header[^>]*>[\s\S]*?<\/header>/gi, "")
    .replace(/<[^>]+>/g, " ")
    .replace(/&nbsp;/g, " ")
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/\s+/g, " ")
    .trim()
}
