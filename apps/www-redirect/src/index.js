export default {
  fetch(request) {
    const url = new URL(request.url);
    url.hostname = "komet.sh";
    return Response.redirect(url.toString(), 301);
  },
};
