import Nav from "@/components/Nav";
import Hero from "@/components/Hero";
import Features from "@/components/Features";
import Topology from "@/components/Topology";
import Stats from "@/components/Stats";
import Downloads from "@/components/Downloads";
import Faq from "@/components/Faq";
import Footer from "@/components/Footer";

export default function Home() {
  return (
    <>
      <Nav />
      <main>
        <Hero />
        <Features />
        <Topology />
        <Stats />
        <Downloads />
        <Faq />
      </main>
      <Footer />
    </>
  );
}
