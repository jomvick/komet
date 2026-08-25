import Nav from "@/components/Nav";
import Hero from "@/components/Hero";
import Features from "@/components/Features";
import AgentsOrbit from "@/components/AgentsOrbit";
import Faq from "@/components/Faq";
import Footer from "@/components/Footer";

export default function Home() {
  return (
    <>
      <Nav />
      <main>
        <Hero />
        <Features />
        <AgentsOrbit />
        <Faq />
      </main>
      <Footer />
    </>
  );
}

